pub mod datetime;
pub mod decimal;
pub mod metamap;
pub mod metamethod;
pub mod rpcvalue;
pub mod rpctype;
pub mod rpcframe;
pub mod rpcmessage;
pub mod util;
pub mod reader;
pub mod writer;
pub mod cpon;
pub mod chainpack;
pub mod connection;
pub mod client;
pub mod shvnode;
pub mod rpc;
pub mod broker;

use std::future::Future;
use async_std::task;
pub use datetime::DateTime;
pub use decimal::Decimal;
pub use metamap::MetaMap;
pub use reader::{Reader, ReadError, ReadResult};
pub use rpcmessage::{RpcMessage, RpcMessageMetaTags};
pub use rpcvalue::{Blob, List, Map, RpcValue};
pub use rpcvalue::Value;
pub use writer::{Writer, WriteResult};

pub use chainpack::{ChainPackReader, ChainPackWriter};
pub use cpon::{CponReader, CponWriter};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub fn spawn_and_log_error<F>(fut: F) -> task::JoinHandle<()>
    where
        F: Future<Output = crate::Result<()>> + Send + 'static,
{
    task::spawn(async move {
        if let Err(e) = fut.await {
            eprintln!("{}", e)
        }
    })
}