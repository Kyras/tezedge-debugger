// Copyright (c) SimpleStaking and Tezedge Contributors
// SPDX-License-Identifier: MIT

use failure::Error;
use riker::actors::*;
use crate::actors::peer_message::{RawPacketMessage, SenderMessage};
use crate::storage::{MessageStore, StoreMessage};
use crate::storage::storage_message::RESTMessage;
use std::collections::HashMap;
use std::net::SocketAddr;

#[derive(Clone)]
/// Argument structure to create new RPC message processor
pub struct RpcArgs {
    pub port: u16,
    pub db: MessageStore,
}

/// Actor for processing RPC calls for controlled node.
pub struct RpcProcessor {
    port: u16,
    requests: HashMap<SocketAddr, RequestParser>,
    responses: HashMap<SocketAddr, ResponseParser>,
    db: MessageStore,
}

impl RpcProcessor {
    /// Create new Processor from given args
    pub fn new(args: RpcArgs) -> Self {
        Self {
            port: args.port,
            db: args.db,
            requests: Default::default(),
            responses: Default::default(),
        }
    }

    /// Get inner request parser for specific host
    fn get_request_parser(&mut self, addr: SocketAddr) -> &mut RequestParser {
        let parser = self.requests.entry(addr);
        parser.or_insert(RequestParser::new())
    }

    /// Get inner response parser for specific host
    fn get_response_parser(&mut self, addr: SocketAddr) -> &mut ResponseParser {
        let parser = self.responses.entry(addr);
        parser.or_insert(ResponseParser::new())
    }

    /// Process RPC message (request or response), and store it if necessary
    fn process_message(&mut self, msg: &RawPacketMessage) -> Result<(), Error> {
        if msg.is_incoming() {
            let parser = self.get_request_parser(msg.remote_addr());
            if let Some(payload) = parser.process_message(msg.payload()) {
                let _ = self.db.store_rpc_message(&StoreMessage::new_rest(msg.remote_addr(), msg.is_incoming(), payload));
                self.requests.remove(&msg.remote_addr());
            }
        } else {
            let parser = self.get_response_parser(msg.remote_addr());
            if let Some(payload) = parser.process_message(msg.payload()) {
                let _ = self.db.store_rpc_message(&StoreMessage::new_rest(msg.remote_addr(), msg.is_incoming(), payload));
                self.responses.remove(&msg.remote_addr());
            }
        }
        Ok(())
    }
}

impl Actor for RpcProcessor {
    type Msg = RawPacketMessage;

    fn recv(&mut self, ctx: &Context<RawPacketMessage>, mut msg: RawPacketMessage, sender: Sender) {
        let _ = self.process_message(&msg);
        if let Some(sender) = sender {
            msg.flip_side();
            if let Err(_) = sender.try_tell(SenderMessage::Relay(msg), ctx.myself()) {
                log::error!("unable to reach packet orchestrator with processed packet")
            }
        }
    }
}

/// Request HTTP Header
pub struct RequestHeader {
    method: String,
    path: String,
}

/// Parser for HTTP request from TCP packet(s)
pub struct RequestParser {
    header: Option<RequestHeader>,
    buffer: String,
    missing: usize,
}

impl RequestParser {
    /// Create new parser for single request
    pub fn new() -> Self {
        Self {
            header: None,
            buffer: Default::default(),
            missing: 0,
        }
    }

    /// Process packet which is part of this request, if it was last, return parsed Request
    pub fn process_message(&mut self, data: &[u8]) -> Option<RESTMessage> {
        if self.header.is_some() {
            self.continue_processing(data)
        } else {
            self.start_processing(data)
        }
    }

    /// If Request was fragmented, continue processing until last packet is received
    fn continue_processing(&mut self, data: &[u8]) -> Option<RESTMessage> {
        if let Ok(str) = std::str::from_utf8(data) {
            self.buffer.push_str(str);
            self.missing = self.missing.saturating_sub(str.len());
            if self.missing == 0 {
                self.flush_buffer()
            } else {
                None
            }
        } else {
            None
        }
    }

    /// If this is a new request process it as if it was segmented
    fn start_processing(&mut self, data: &[u8]) -> Option<RESTMessage> {
        if data.len() == 0 {
            None
        } else {
            let string = std::str::from_utf8(data).ok()?;
            let data_iter: Vec<&str> = string.splitn(2, "\r\n\r\n").collect();
            if data_iter.len() < 2 {
                // Not valid HTTP header (should be <METADATA+HEADERS>\r\n\r\n<PAYLOAD>)
                // Not valid HTTP request
                return None;
            } else {
                // Process metadata + header
                let meta = data_iter[0];
                let meta_iter: Vec<&str> = meta.splitn(2, "\r\n").collect();
                if meta_iter.len() < 2 {
                    // Not valid HTTP header (should be <METADATA>\r\n<HEADERS>)
                    return None;
                } else {
                    let info = meta_iter[0];
                    let metadata: Vec<&str> = info.splitn(3, ' ').collect();
                    if metadata.len() < 3 {
                        // Not valid HTTP metadata (should be <METHOD> <URI> HTTP<VERSION>)
                        return None;
                    } else {
                        let method = metadata[0].to_string();
                        let path = metadata[1].to_string();
                        let headers: HashMap<String, &str> = meta_iter[1]
                            .split("\r\n")
                            .filter(|x| x.contains(":"))
                            .map(|x| {
                                let vals: Vec<&str> = x.splitn(2, ":").collect();
                                (vals[0].trim().to_lowercase(), vals[1].trim())
                            })
                            .collect();
                        self.missing = headers.get("content-length").unwrap_or(&"0").parse().ok()?;
                        self.header = Some(RequestHeader {
                            method,
                            path,
                        });
                        self.continue_processing(data_iter[1].as_bytes())
                    }
                }
            }
        }
    }

    /// Flush buffer and finish parsing
    fn flush_buffer(&mut self) -> Option<RESTMessage> {
        if let Some(header) = std::mem::replace(&mut self.header, None) {
            Some(RESTMessage::Request {
                method: header.method,
                path: header.path,
                payload: std::mem::replace(&mut self.buffer, Default::default()),
            })
        } else {
            self.clean_buffer();
            None
        }
    }

    /// Remove all data from inner buffers
    fn clean_buffer(&mut self) {
        self.buffer.clear()
    }
}

/// Response HTTP header
pub struct ResponseHeader {
    status: String,
}

/// Parser for  HTTP response from TCP packet(s)
pub struct ResponseParser {
    header: Option<ResponseHeader>,
    buffer: String,
    missing: usize,
}

impl ResponseParser {
    /// Create new http response parser
    pub fn new() -> Self {
        Self {
            header: None,
            buffer: Default::default(),
            missing: 0,
        }
    }

    /// Process packet which is part of this response, if it was last, return parsed Request
    pub fn process_message(&mut self, data: &[u8]) -> Option<RESTMessage> {
        if self.header.is_some() {
            self.continue_processing(data)
        } else {
            self.start_processing(data)
        }
    }

    /// If response was fragmented, continue processing until last packet is received
    fn continue_processing(&mut self, data: &[u8]) -> Option<RESTMessage> {
        if let Ok(str) = std::str::from_utf8(data) {
            self.buffer.push_str(str);
            self.missing = self.missing.saturating_sub(str.len());
            if self.missing == 0 {
                self.flush_buffer()
            } else {
                None
            }
        } else {
            None
        }
    }

    /// If this is a new response process it as if it was segmented
    fn start_processing(&mut self, data: &[u8]) -> Option<RESTMessage> {
        if data.len() == 0 {
            None
        } else {
            let string = std::str::from_utf8(data).ok()?;
            let data_iter: Vec<&str> = string.splitn(2, "\r\n\r\n").collect();
            if data_iter.len() < 2 {
                // Not valid HTTP header (should be <METADATA+HEADERS>\r\n\r\n<PAYLOAD>)
                // Not valid HTTP request
                return None;
            } else {
                // Process metadata + header
                let meta = data_iter[0];
                let meta_iter: Vec<&str> = meta.splitn(2, "\r\n").collect();
                if meta_iter.len() < 2 {
                    // Not valid HTTP header (should be <METADATA>\r\n<HEADERS>)
                    return None;
                } else {
                    let info = meta_iter[0];
                    let metadata: Vec<&str> = info.splitn(3, ' ').collect();
                    if metadata.len() < 3 {
                        // Not valid HTTP metadata (should be HTTP/<VERSION> <STATUS_NUMBER> <STATUS_DESCRIPTION>)
                        return None;
                    } else {
                        let status = metadata[1].trim();
                        let headers: HashMap<String, &str> = meta_iter[1]
                            .split("\r\n")
                            .filter(|x| x.contains(":"))
                            .map(|x| {
                                let vals: Vec<&str> = x.splitn(2, ":").collect();
                                (vals[0].trim().to_lowercase(), vals[1].trim())
                            })
                            .collect();
                        self.missing = headers.get("content-length").unwrap_or(&"0").parse().ok()?;
                        self.header = Some(ResponseHeader { status: status.to_string() });
                        self.continue_processing(data_iter[1].as_bytes())
                    }
                }
            }
        }
    }

    /// Flush buffers and finalize parsing, if possible
    fn flush_buffer(&mut self) -> Option<RESTMessage> {
        if let Some(header) = std::mem::replace(&mut self.header, None) {
            Some(RESTMessage::Response {
                status: header.status,
                payload: std::mem::replace(&mut self.buffer, Default::default()),
            })
        } else {
            self.clean_buffer();
            None
        }
    }

    /// Remove all data from buffers
    fn clean_buffer(&mut self) {
        self.buffer.clear()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request() {
        let http = "474554202f636861696e732f6d61696e2f626c6f636b732f6865616420485454502f312e310d0a486f73743a2070726f642e74657a656467652e636f6d3a31383733320d0a557365722d4167656e743a206375726c2f372e36352e330d0a4163636570743a202a2f2a0d0a0d0a".to_string();
        let bytes = hex::decode(&http).unwrap();
        let mut parser = RequestParser::new();
        let msg = parser.process_message(&bytes);
        assert!(msg.is_some());
        let msg = msg.unwrap();
        match msg {
            RESTMessage::Request { method, path, payload } => {
                assert_eq!(method, "GET");
                assert_eq!(path, "/chains/main/blocks/head");
                assert_eq!(payload, "");
            }
            RESTMessage::Response { .. } => assert!(false, "Expected Request message, got response.")
        }
    }

    #[test]
    fn parse_response() {
        let http = "485454502f312e3120323030204f4b0d0a636f6e74656e742d747970653a206170706c69636174696f6e2f6a736f6e0d0a6163636573732d636f6e74726f6c2d616c6c6f772d6f726967696e3a202a0d0a636f6e74656e742d6c656e6774683a2034320d0a646174653a205475652c2032382041707220323032302030383a32353a353120474d540d0a0d0a223266396632646565343736313333396234366666396634313335616231356136383332636335316522".to_string();
        let bytes = hex::decode(&http).unwrap();
        let mut parser = ResponseParser::new();
        let msg = parser.process_message(&bytes);
        assert!(msg.is_some());
        let msg = msg.unwrap();
        match msg {
            RESTMessage::Response { status, payload } => {
                assert_eq!(status, "200");
                assert_eq!(payload, "\"2f9f2dee4761339b46ff9f4135ab15a6832cc51e\"");
            }
            RESTMessage::Request { .. } => assert!(false, "Expected Response message, got Request.")
        }
    }

    #[test]
    fn parse_segmented_response() {
        let part1 = "485454502f312e3120323030204f4b0d0a636f6e74656e742d747970653a206170706c69636174696f6e2f6a736f6e0d0a6163636573732d636f6e74726f6c2d616c6c6f772d6f726967696e3a202a0d0a636f6e74656e742d6c656e6774683a2031313939310d0a646174653a205475652c2032382041707220323032302030383a32393a343720474d540d0a0d0a7b2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c22636861696e5f6964223a224e6574586a443348504a4a6a6d6364222c2268617368223a22424c4344794e535931336452474654386179637a6f5050714c7a566e34695a6a6e774c6763664c6a71654648696d626b41394e222c22686561646572223a7b22636f6e74657874223a22436f574a7046444d6a3755635461327a586a6b6d75335463784e644e66784d456b4b3659776b6f4e6d45386a447572356e773645222c226c6576656c223a39303336392c226669746e657373223a5b223031222c2230303030303030303030303136313030225d2c2270726f6f665f6f665f776f726b5f6e6f6e6365223a2262316137623932623835316530333030222c2274696d657374616d70223a22323032302d30312d30365431313a34363a34395a222c2276616c69646174696f6e5f70617373223a342c227072656465636573736f72223a22424c7a79374d34515a6343433571686f684b3647367741576d4c5570694b4838623541333148516552644258396b6666534234222c227369676e6174757265223a2273696758376d5962634a486a6d55594d566446516a6b6b745135346131484c4737457343566f7a5a364c5439466b597a756b6d7a706b6e7046664c535543335a703548684a796a705737595a547071534672706f6a473475566737314d50797a222c226f7065726174696f6e735f68617368223a224c4c6f5a686e4a38566478797272427a6a6d345342316d39437866327378336b59686831634c70704179617670447772577051667a222c227072696f72697479223a302c2270726f746f223a327d2c226d65746164617461223a7b226c6576656c223a7b226379636c65223a34342c226379636c655f706f736974696f6e223a3235362c2265787065637465645f636f6d6d69746d656e74223a66616c73652c226c6576656c223a39303336392c226c6576656c5f706f736974696f6e223a39303336382c22766f74696e675f706572696f64223a34342c22766f74696e675f706572696f645f706f736974696f6e223a3235367d2c2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c226d61785f6f7065726174696f6e5f646174615f6c656e677468223a31363338342c226e6f6e63655f68617368223a6e756c6c2c226d61785f626c6f636b5f6865616465725f6c656e677468223a3233382c22636f6e73756d65645f676173223a223230343134222c226465616374697661746564223a5b5d2c2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d333532303030303030222c22636f6e7472616374223a22747a314e5254516571637577796267725a664a61764259336f6638337538754c7046426a222c226b696e64223a22636f6e7472616374227d2c7b2263617465676f7279223a226465706f73697473222c226368616e6765223a22333532303030303030222c226379636c65223a34342c2264656c6567617465223a22747a314e5254516571637577796267725a664a61764259336f6638337538754c7046426a222c226b696e64223a22667265657a6572227d2c7b2263617465676f7279223a2272657761726473222c22".to_string();
        let part2 = "6368616e6765223a223337353030303030222c226379636c65223a34342c2264656c6567617465223a22747a314e5254516571637577796267725a664a61764259336f6638337538754c7046426a222c226b696e64223a22667265657a6572227d5d2c22766f74696e675f706572696f645f6b696e64223a2270726f706f73616c222c22746573745f636861696e5f737461747573223a7b22737461747573223a226e6f745f72756e6e696e67227d2c226e6578745f70726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c226d61785f6f7065726174696f6e5f6c6973745f6c656e677468223a5b7b226d61785f6f70223a33322c226d61785f73697a65223a33323736387d2c7b226d61785f73697a65223a33323736387d2c7b226d61785f6f70223a3133322c226d61785f73697a65223a3133353136387d2c7b226d61785f73697a65223a3532343238387d5d2c226d61785f6f7065726174696f6e735f74746c223a36302c2262616b6572223a22747a314e5254516571637577796267725a664a61764259336f6638337538754c7046426a227d2c226f7065726174696f6e73223a5b5b7b2268617368223a226f705a536543686274346a726f78547851667576517a31646843434a396d554b6832415731544b367536486e41386a616e6d53222c22636f6e74656e7473223a5b7b226b696e64223a22656e646f7273656d656e74222c226c6576656c223a39303336382c226d65746164617461223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d3838303030303030222c22636f6e7472616374223a22747a314d6a557a6369653758754b58517454546a63416d555163726353557174626e3438222c226b696e64223a22636f6e7472616374227d2c7b2263617465676f7279223a226465706f73697473222c226368616e6765223a223838303030303030222c226379636c65223a34342c2264656c6567617465223a22747a314d6a557a6369653758754b58517454546a63416d555163726353557174626e3438222c226b696e64223a22667265657a6572227d2c7b2263617465676f7279223a2272657761726473222c226368616e6765223a2232353030303030222c226379636c65223a34342c2264656c6567617465223a22747a314d6a557a6369653758754b58517454546a63416d555163726353557174626e3438222c226b696e64223a22667265657a6572227d5d2c2264656c6567617465223a22747a314d6a557a6369653758754b58517454546a63416d555163726353557174626e3438222c22736c6f7473223a5b32392c365d7d7d5d2c227369676e6174757265223a2273696767746d6256597050417a4d507864357554614e53735633615279677a644d68356b556a365a72315259476b766f5133456b446b33575461384e45336e5975336f7964515a74677063316f3173725a31733469714d4656317a5332625135222c226272616e6368223a22424c7a79374d34515a6343433571686f684b3647367741576d4c5570694b4838623541333148516552644258396b6666534234222c2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c22636861696e5f6964223a224e6574586a443348504a4a6a6d6364227d2c7b2268617368223a226f6f4e36726f463631384c446f6a7438696d5a644171714757523468796d70565453516f34546e73736f51416a63713166376d222c22636861696e5f6964223a224e6574586a443348504a4a6a6d6364222c227369676e6174757265223a22736967714d676b5952376146766d7657596b556b513276583876".to_string();
        let part3 = "33366237557a794b34556e4543506762314e4b774c77535a70764251316e42745454657666397a7535694668634e73706f676a4234564552387473773366787439375156466a222c22636f6e74656e7473223a5b7b226b696e64223a22656e646f7273656d656e74222c226c6576656c223a39303336382c226d65746164617461223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d333038303030303030222c22636f6e7472616374223a22747a31506972626f5a4b4656716b6645343568564c706b7058615a744c6b336d71433137222c226b696e64223a22636f6e7472616374227d2c7b2263617465676f7279223a226465706f73697473222c226368616e6765223a22333038303030303030222c226379636c65223a34342c2264656c6567617465223a22747a31506972626f5a4b4656716b6645343568564c706b7058615a744c6b336d71433137222c226b696e64223a22667265657a6572227d2c7b2263617465676f7279223a2272657761726473222c226368616e6765223a2238373530303030222c226379636c65223a34342c2264656c6567617465223a22747a31506972626f5a4b4656716b6645343568564c706b7058615a744c6b336d71433137222c226b696e64223a22667265657a6572227d5d2c2264656c6567617465223a22747a31506972626f5a4b4656716b6645343568564c706b7058615a744c6b336d71433137222c22736c6f7473223a5b32332c31392c31362c31352c31342c392c375d7d7d5d2c2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c226272616e6368223a22424c7a79374d34515a6343433571686f684b3647367741576d4c5570694b4838623541333148516552644258396b6666534234227d2c7b2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c2268617368223a226f6f7a585a313767416765796954354c62544a7167546a46655a68736538656e5a5931696439747653797379616472386f7573222c22636f6e74656e7473223a5b7b226b696e64223a22656e646f7273656d656e74222c226c6576656c223a39303336382c226d65746164617461223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d313736303030303030222c22636f6e7472616374223a22747a314e5254516571637577796267725a664a61764259336f6638337538754c7046426a222c226b696e64223a22636f6e7472616374227d2c7b2263617465676f7279223a226465706f73697473222c226368616e6765223a22313736303030303030222c226379636c65223a34342c2264656c6567617465223a22747a314e5254516571637577796267725a664a61764259336f6638337538754c7046426a222c226b696e64223a22667265657a6572227d2c7b2263617465676f7279223a2272657761726473222c226368616e6765223a2235303030303030222c226379636c65223a34342c2264656c6567617465223a22747a314e5254516571637577796267725a664a61764259336f6638337538754c7046426a222c226b696e64223a22667265657a6572227d5d2c2264656c6567617465223a22747a314e5254516571637577796267725a664a61764259336f6638337538754c7046426a222c22736c6f7473223a5b33312c32372c31332c31315d7d7d5d2c226272616e6368223a22424c7a79374d34515a6343433571686f684b3647367741576d4c5570694b4838623541333148516552644258396b6666534234222c22636861696e5f6964223a224e6574586a443348504a4a6a6d6364222c22".to_string();
        let part4 = "7369676e6174757265223a2273696755716555506e6b44344a657370705561415942355362325244437a34537774484a647a336753726a767a68476f4233626b33644b46336e4c596f4a4d7243335638437a354b573244514b73333233543334314338417833486d62763132227d2c7b2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c226272616e6368223a22424c7a79374d34515a6343433571686f684b3647367741576d4c5570694b4838623541333148516552644258396b6666534234222c227369676e6174757265223a227369677656614862654a3157596a457764543433516b3639677977444e7948437061765355656b6e3368617578693677325856364759386238554a76737a6d565535516978757570576e45366661676e4b3936387064456d5378346d59776248222c2268617368223a226f6f794d6575415051466f43374c63643354754c344378545264796277555438353170757445467933593455424d7a4d716754222c22636861696e5f6964223a224e6574586a443348504a4a6a6d6364222c22636f6e74656e7473223a5b7b226b696e64223a22656e646f7273656d656e74222c226c6576656c223a39303336382c226d65746164617461223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d3838303030303030222c22636f6e7472616374223a22747a315948324c45367037536a3136764636697266485839325156343558415a59486e58222c226b696e64223a22636f6e7472616374227d2c7b2263617465676f7279223a226465706f73697473222c226368616e6765223a223838303030303030222c226379636c65223a34342c2264656c6567617465223a22747a315948324c45367037536a3136764636697266485839325156343558415a59486e58222c226b696e64223a22667265657a6572227d2c7b2263617465676f7279223a2272657761726473222c226368616e6765223a2232353030303030222c226379636c65223a34342c2264656c6567617465223a22747a315948324c45367037536a3136764636697266485839325156343558415a59486e58222c226b696e64223a22667265657a6572227d5d2c2264656c6567617465223a22747a315948324c45367037536a3136764636697266485839325156343558415a59486e58222c22736c6f7473223a5b32362c355d7d7d5d7d2c7b2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c22636861696e5f6964223a224e6574586a443348504a4a6a6d6364222c2268617368223a226f6f6377447a7833514d7231324d485032687439476462584b3879396753546377654b5a663350336a5747695074724a6b544e222c227369676e6174757265223a2273696777426758767045727a6d456a7964343133624e3665785a3145794a374445534c413537485a63654a656b5477376d346a624c414b356e4d6d37585658645a735a71314134356d5a35374c547269766446656a717931594d6b3632597654222c22636f6e74656e7473223a5b7b226b696e64223a22656e646f7273656d656e74222c226c6576656c223a39303336382c226d65746164617461223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d3434303030303030222c22636f6e7472616374223a22747a31546f4456544563695957324469344d6b664643626f4d7a4a4479625062536a5976222c226b696e64223a22636f6e7472616374227d2c7b2263617465676f7279223a226465706f73697473222c226368616e6765223a223434303030303030222c226379636c".to_string();
        let part5 = "65223a34342c2264656c6567617465223a22747a31546f4456544563695957324469344d6b664643626f4d7a4a4479625062536a5976222c226b696e64223a22667265657a6572227d2c7b2263617465676f7279223a2272657761726473222c226368616e6765223a2231323530303030222c226379636c65223a34342c2264656c6567617465223a22747a31546f4456544563695957324469344d6b664643626f4d7a4a4479625062536a5976222c226b696e64223a22667265657a6572227d5d2c2264656c6567617465223a22747a31546f4456544563695957324469344d6b664643626f4d7a4a4479625062536a5976222c22736c6f7473223a5b31325d7d7d5d2c226272616e6368223a22424c7a79374d34515a6343433571686f684b3647367741576d4c5570694b4838623541333148516552644258396b6666534234227d2c7b227369676e6174757265223a22736967514a56664e43414e69384a426d647163535565366132314b74443172455231734d6a4d4b465a484464376631766870616b4b57524b69763669427275354a48617366786b736262374a317a38417843674d79784c397231667355386434222c2268617368223a226f6f467346326b746932476a43336a76635a3633576433725576755a513877555a346173427646577758737738547531346f41222c226272616e6368223a22424c7a79374d34515a6343433571686f684b3647367741576d4c5570694b4838623541333148516552644258396b6666534234222c2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c22636861696e5f6964223a224e6574586a443348504a4a6a6d6364222c22636f6e74656e7473223a5b7b226b696e64223a22656e646f7273656d656e74222c226c6576656c223a39303336382c226d65746164617461223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d3838303030303030222c22636f6e7472616374223a22747a3161575850323337424c774e484a6343443462334475744365766871713254315a39222c226b696e64223a22636f6e7472616374227d2c7b2263617465676f7279223a226465706f73697473222c226368616e6765223a223838303030303030222c226379636c65223a34342c2264656c6567617465223a22747a3161575850323337424c774e484a6343443462334475744365766871713254315a39222c226b696e64223a22667265657a6572227d2c7b2263617465676f7279223a2272657761726473222c226368616e6765223a2232353030303030222c226379636c65223a34342c2264656c6567617465223a22747a3161575850323337424c774e484a6343443462334475744365766871713254315a39222c226b696e64223a22667265657a6572227d5d2c2264656c6567617465223a22747a3161575850323337424c774e484a6343443462334475744365766871713254315a39222c22736c6f7473223a5b32382c32355d7d7d5d7d2c7b2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c2268617368223a226f70394d3253795a7a725872786b584748544d7133537a6d6b62653475666f4b39314165343362574576487731367644453372222c22636861696e5f6964223a224e6574586a443348504a4a6a6d6364222c226272616e6368223a22424c7a79374d34515a6343433571686f684b3647367741576d4c5570694b4838623541333148516552644258396b6666534234222c22636f6e74656e7473223a5b7b226b696e64223a22656e646f7273656d656e74222c226c6576656c223a39303336382c226d65".to_string();
        let part6 = "746164617461223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d313332303030303030222c22636f6e7472616374223a22747a3154455a74596e754c695a4c64413663374a797341554a63484d726f677534437072222c226b696e64223a22636f6e7472616374227d2c7b2263617465676f7279223a226465706f73697473222c226368616e6765223a22313332303030303030222c226379636c65223a34342c2264656c6567617465223a22747a3154455a74596e754c695a4c64413663374a797341554a63484d726f677534437072222c226b696e64223a22667265657a6572227d2c7b2263617465676f7279223a2272657761726473222c226368616e6765223a2233373530303030222c226379636c65223a34342c2264656c6567617465223a22747a3154455a74596e754c695a4c64413663374a797341554a63484d726f677534437072222c226b696e64223a22667265657a6572227d5d2c2264656c6567617465223a22747a3154455a74596e754c695a4c64413663374a797341554a63484d726f677534437072222c22736c6f7473223a5b33302c382c305d7d7d5d2c227369676e6174757265223a227369676b4b675a6d6664796a51613369586e35644d3772516a75706f5332776b625a75666f7a36596e787168415376434c6e63756d454e4e6a4a6a3566734b4b36364d4c786a5a4166504e733359586641723459655873665a776f6d326e4c4e227d2c7b2268617368223a226f6f7657566548695a576d505a374345534732373147704b487a6d566f545755334867777569676d67554a5067566556326636222c2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c22636f6e74656e7473223a5b7b226b696e64223a22656e646f7273656d656e74222c226c6576656c223a39303336382c226d65746164617461223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d313332303030303030222c22636f6e7472616374223a22747a31526f6d6169574a56334e46445a57544d565232614565486b6e736e336946354769222c226b696e64223a22636f6e7472616374227d2c7b2263617465676f7279223a226465706f73697473222c226368616e6765223a22313332303030303030222c226379636c65223a34342c2264656c6567617465223a22747a31526f6d6169574a56334e46445a57544d565232614565486b6e736e336946354769222c226b696e64223a22667265657a6572227d2c7b2263617465676f7279223a2272657761726473222c226368616e6765223a2233373530303030222c226379636c65223a34342c2264656c6567617465223a22747a31526f6d6169574a56334e46445a57544d565232614565486b6e736e336946354769222c226b696e64223a22667265657a6572227d5d2c2264656c6567617465223a22747a31526f6d6169574a56334e46445a57544d565232614565486b6e736e336946354769222c22736c6f7473223a5b32322c342c335d7d7d5d2c22636861696e5f6964223a224e6574586a443348504a4a6a6d6364222c226272616e6368223a22424c7a79374d34515a6343433571686f684b3647367741576d4c5570694b4838623541333148516552644258396b6666534234222c227369676e6174757265223a2273696766646d4e4343753972624179387a546e785038534b77514a3343636837525a6d376572504b324b4473445242484d7737334338565261786b636f6b374c487a6e5a366466514e4c36567a3831426746347265766f747033653532504564227d2c7b2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b53527067".to_string();
        let part7 = "6e445939383261396f59735358524c514562222c226272616e6368223a22424c7a79374d34515a6343433571686f684b3647367741576d4c5570694b4838623541333148516552644258396b6666534234222c22636f6e74656e7473223a5b7b226b696e64223a22656e646f7273656d656e74222c226c6576656c223a39303336382c226d65746164617461223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d323230303030303030222c22636f6e7472616374223a22747a314b7a36565345504e6e4b50694e766879696f3645316f7462536444685644397142222c226b696e64223a22636f6e7472616374227d2c7b2263617465676f7279223a226465706f73697473222c226368616e6765223a22323230303030303030222c226379636c65223a34342c2264656c6567617465223a22747a314b7a36565345504e6e4b50694e766879696f3645316f7462536444685644397142222c226b696e64223a22667265657a6572227d2c7b2263617465676f7279223a2272657761726473222c226368616e6765223a2236323530303030222c226379636c65223a34342c2264656c6567617465223a22747a314b7a36565345504e6e4b50694e766879696f3645316f7462536444685644397142222c226b696e64223a22667265657a6572227d5d2c2264656c6567617465223a22747a314b7a36565345504e6e4b50694e766879696f3645316f7462536444685644397142222c22736c6f7473223a5b32312c31382c31372c322c315d7d7d5d2c22636861696e5f6964223a224e6574586a443348504a4a6a6d6364222c227369676e6174757265223a227369676674507463534870375076355567486b784d71565272486e62634a334c594d61443661756150716d36536f63635257384871434a47454271753534726645527043697a36516351315638366739514a766b7a6e705a32656e6e63536257222c2268617368223a226f6e7632364a5270687a615a6168644847617a394859465263393468706f7831613750424637787833547a5436327931435157227d2c7b22636f6e74656e7473223a5b7b226b696e64223a22656e646f7273656d656e74222c226c6576656c223a39303336382c226d65746164617461223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d3434303030303030222c22636f6e7472616374223a22747a314b78467079597a6b6367713543416f6b38664e76737a72526344324d55464d5879222c226b696e64223a22636f6e7472616374227d2c7b2263617465676f7279223a226465706f73697473222c226368616e6765223a223434303030303030222c226379636c65223a34342c2264656c6567617465223a22747a314b78467079597a6b6367713543416f6b38664e76737a72526344324d55464d5879222c226b696e64223a22667265657a6572227d2c7b2263617465676f7279223a2272657761726473222c226368616e6765223a2231323530303030222c226379636c65223a34342c2264656c6567617465223a22747a314b78467079597a6b6367713543416f6b38664e76737a72526344324d55464d5879222c226b696e64223a22667265657a6572227d5d2c2264656c6567617465223a22747a314b78467079597a6b6367713543416f6b38664e76737a72526344324d55464d5879222c22736c6f7473223a5b32345d7d7d5d2c2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c22636861696e5f6964223a224e6574586a443348504a4a6a6d6364222c2268617368223a226f705974634d74416d317741645372735967316b634d4c4a5331726b6a4242424a565570585a64344d72656a76677a78737a".to_string();
        let part8 = "58222c226272616e6368223a22424c7a79374d34515a6343433571686f684b3647367741576d4c5570694b4838623541333148516552644258396b6666534234222c227369676e6174757265223a22736967723451354739424c68716f38534d6550326a37444b664267314a6632626a534d4d4c70417739746942456565354d417163654841724845447a737254466a5361783359444346556f63526441394242573535657637486f39474b5a746d227d5d2c5b5d2c5b5d2c5b7b227369676e6174757265223a22736967646641584e67715a3142515977636655795a43686b4a366d627a645a3470614c3670483133526e724b636a544b6a4b33457235344a4b334c4751573574754c46653554317333334772765376425873475657566e424543525953355a76222c2268617368223a226f6e723570326654714e754c546731576a5843634b4543354850764a54705671733858443771354c714e48454d553267486a4c222c2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c22636861696e5f6964223a224e6574586a443348504a4a6a6d6364222c226272616e6368223a22424c7a79374d34515a6343433571686f684b3647367741576d4c5570694b4838623541333148516552644258396b6666534234222c22636f6e74656e7473223a5b7b22616d6f756e74223a2237333730222c22636f756e746572223a223639373039222c2264657374696e6174696f6e223a22747a3159387a64745665327757653751644e546e416477426365715942436441334a6a38222c22666565223a2231323832222c226761735f6c696d6974223a223130333037222c226b696e64223a227472616e73616374696f6e222c226d65746164617461223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d31323832222c22636f6e7472616374223a22747a3154455a74596e754c695a4c64413663374a797341554a63484d726f677534437072222c226b696e64223a22636f6e7472616374227d2c7b2263617465676f7279223a2266656573222c226368616e6765223a2231323832222c226379636c65223a34342c2264656c6567617465223a22747a314e5254516571637577796267725a664a61764259336f6638337538754c7046426a222c226b696e64223a22667265657a6572227d5d2c226f7065726174696f6e5f726573756c74223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d37333730222c22636f6e7472616374223a22747a3154455a74596e754c695a4c64413663374a797341554a63484d726f677534437072222c226b696e64223a22636f6e7472616374227d2c7b226368616e6765223a2237333730222c22636f6e7472616374223a22747a3159387a64745665327757653751644e546e416477426365715942436441334a6a38222c226b696e64223a22636f6e7472616374227d5d2c22636f6e73756d65645f676173223a223130323037222c22737461747573223a226170706c696564227d7d2c22736f75726365223a22747a3154455a74596e754c695a4c64413663374a797341554a63484d726f677534437072222c2273746f726167655f6c696d6974223a2230227d5d7d2c7b22636861696e5f6964223a224e6574586a443348504a4a6a6d6364222c227369676e6174757265223a22736967626f4661444a774a6a367947315a553762467a426a657276744e386f597475794c5a6d436d44613647735054416f567636716b765855756a70647368596955353968524d584b66617a4e36656b54317731464751666d4e397542387959222c226272616e6368223a22424c7a79374d34515a6343433571686f684b3647367741576d4c".to_string();
        let part9 = "5570694b4838623541333148516552644258396b6666534234222c2270726f746f636f6c223a22507343415254484147617a4b6248746e4b664c7a5167336b6d7335326b535270676e445939383261396f59735358524c514562222c2268617368223a226f6e66786546744c32384b783566554d4d4c6f4e6d6539764e3378544a484d4339716e796a506d744c366a72544775564c3462222c22636f6e74656e7473223a5b7b22616d6f756e74223a2238333530222c22636f756e746572223a223730313139222c2264657374696e6174696f6e223a22747a3154455a74596e754c695a4c64413663374a797341554a63484d726f677534437072222c22666565223a2231323832222c226761735f6c696d6974223a223130333037222c226b696e64223a227472616e73616374696f6e222c226d65746164617461223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d31323832222c22636f6e7472616374223a22747a3159387a64745665327757653751644e546e416477426365715942436441334a6a38222c226b696e64223a22636f6e7472616374227d2c7b2263617465676f7279223a2266656573222c226368616e6765223a2231323832222c226379636c65223a34342c2264656c6567617465223a22747a314e5254516571637577796267725a664a61764259336f6638337538754c7046426a222c226b696e64223a22667265657a6572227d5d2c226f7065726174696f6e5f726573756c74223a7b2262616c616e63655f75706461746573223a5b7b226368616e6765223a222d38333530222c22636f6e7472616374223a22747a3159387a64745665327757653751644e546e416477426365715942436441334a6a38222c226b696e64223a22636f6e7472616374227d2c7b226368616e6765223a2238333530222c22636f6e7472616374223a22747a3154455a74596e754c695a4c64413663374a797341554a63484d726f677534437072222c226b696e64223a22636f6e7472616374227d5d2c22636f6e73756d65645f676173223a223130323037222c22737461747573223a226170706c696564227d7d2c22736f75726365223a22747a3159387a64745665327757653751644e546e416477426365715942436441334a6a38222c2273746f726167655f6c696d6974223a2230227d5d7d5d5d7d".to_string();
        let mut parser = ResponseParser::new();
        let bytes = hex::decode(part1).unwrap();
        let ret = parser.process_message(&bytes);
        assert!(ret.is_none(), "Should not finish after processing part 1");
        let bytes = hex::decode(part2).unwrap();
        let ret = parser.process_message(&bytes);
        assert!(ret.is_none(), "Should not finish after processing part 2");
        let bytes = hex::decode(part3).unwrap();
        let ret = parser.process_message(&bytes);
        assert!(ret.is_none(), "Should not finish after processing part 3");
        let bytes = hex::decode(part4).unwrap();
        let ret = parser.process_message(&bytes);
        assert!(ret.is_none(), "Should not finish after processing part 4");
        let bytes = hex::decode(part5).unwrap();
        let ret = parser.process_message(&bytes);
        assert!(ret.is_none(), "Should not finish after processing part 5");
        let bytes = hex::decode(part6).unwrap();
        let ret = parser.process_message(&bytes);
        assert!(ret.is_none(), "Should not finish after processing part 6");
        let bytes = hex::decode(part7).unwrap();
        let ret = parser.process_message(&bytes);
        assert!(ret.is_none(), "Should not finish after processing part 7");
        let bytes = hex::decode(part8).unwrap();
        let ret = parser.process_message(&bytes);
        assert!(ret.is_none(), "Should not finish after processing part 8");
        let bytes = hex::decode(part9).unwrap();
        let ret = parser.process_message(&bytes);
        assert!(ret.is_some(), "Should have finished after processing part 9");
        let msg = ret.unwrap();
        match msg {
            RESTMessage::Response { status, payload } => {
                assert_eq!(status, "200");
                assert_eq!(payload.len(), 11991);
            }
            RESTMessage::Request { .. } => assert!(false, "Expected Response message, got Request.")
        }
    }
}