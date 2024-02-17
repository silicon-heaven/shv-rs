use std::fmt;
use std::io::{BufReader};
use crate::{ChainPackReader, ChainPackWriter, MetaMap, RpcMessage, RpcMessageMetaTags, rpctype, RpcValue};
use crate::writer::Writer;
use crate::reader::Reader;
use crate::CponReader;

#[derive(Clone, Debug, PartialEq)]
pub struct RpcFrame {
    pub protocol: Protocol,
    pub meta: MetaMap,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Protocol {
    ResetSession = 0,
    ChainPack = 1,
    Cpon = 2,
}
impl fmt::Display for Protocol {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Protocol::Cpon => write!(fmt, "{}", "Cpon"),
            Protocol::ChainPack => write!(fmt, "{}", "ChainPack"),
            Protocol::ResetSession => write!(fmt, "{}", "ResetSession"),
        }
    }
}
impl RpcFrame {
    pub fn new(protocol: Protocol, meta: MetaMap, data: Vec<u8>) -> RpcFrame {
        RpcFrame { protocol, meta, data }
    }
    pub fn from_rpcmessage(msg: &RpcMessage) -> crate::Result<RpcFrame> {
        Self::from_rpcmessage2(msg, Protocol::ChainPack)
    }
    pub fn from_rpcmessage2(msg: &RpcMessage, protocol: Protocol) -> crate::Result<RpcFrame> {
        let mut data = Vec::new();
        if protocol == Protocol::Cpon {
            let mut wr = crate::CponWriter::new(&mut data);
            wr.write_value(&msg.as_rpcvalue().value())?;
        } else {
            let mut wr = ChainPackWriter::new(&mut data);
            wr.write_value(&msg.as_rpcvalue().value())?;
        }
        let meta = msg.as_rpcvalue().meta().clone();
        Ok(RpcFrame { protocol, meta, data })
    }
    pub fn to_rpcmesage(&self) -> crate::Result<RpcMessage> {
        let mut buff = BufReader::new(&*self.data);
        let value;
        match &self.protocol {
            Protocol::ChainPack => {
                let mut rd = ChainPackReader::new(&mut buff);
                value = rd.read_value()?;
            }
            Protocol::Cpon => {
                let mut rd = CponReader::new(&mut buff);
                value = rd.read_value()?;
            }
            _ => {
                return Err("Invalid protocol".into());
            }
        }
        Ok(RpcMessage::from_rpcvalue(RpcValue::new(value, Some(self.meta.clone())))?)
    }
    pub fn prepare_response_meta(src: &MetaMap) -> Result<MetaMap, &'static str> {
        if src.is_request() {
            if let Some(rqid) = src.request_id() {
                let mut dest = MetaMap::new();
                dest.insert(rpctype::Tag::MetaTypeId as i32, RpcValue::from(rpctype::GlobalNS::MetaTypeID::ChainPackRpcMessage as i32));
                dest.set_request_id(rqid);
                dest.set_caller_ids(&src.caller_ids());
                return Ok(dest)
            }
            return Err("Request ID is missing")
        }
        Err("Not RPC Request")
    }
}
impl fmt::Display for RpcFrame {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        if self.protocol == Protocol::ResetSession {
            write!(fmt, "RESET_SESSION")
        } else {
            write!(fmt, "{}", self.meta)?;
            if self.data.len() > 256 {
                write!(fmt, "[ ... {} bytes of data ... ]", self.data.len())
            } else {
                match RpcValue::from_chainpack(&self.data) {
                    Ok(rv) => {
                        write!(fmt, "{}", rv.to_cpon())
                    }
                    Err(e) => {
                        write!(fmt, "[ unpack error: {} ]", e)
                    }
                }
            }
        }
    }
}

impl RpcMessageMetaTags for RpcFrame {
    type Target = RpcFrame;

    fn tag(&self, id: i32) -> Option<&RpcValue> {
        self.meta.tag(id)
    }
    fn set_tag(&mut self, id: i32, val: Option<RpcValue>) -> &mut Self::Target {
        self.meta.set_tag(id, val);
        self
    }
}