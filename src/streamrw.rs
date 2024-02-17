use std::io::{BufReader};
use async_trait::async_trait;
use crate::rpcframe::{Protocol, RpcFrame};
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use log::*;
use crate::{ChainPackReader, ChainPackWriter, Reader, ReadError};
use crate::framerw::{FrameReader, FrameWriter, serialize_meta};
use crate::reader::ReadErrorReason;

pub struct StreamFrameReader<R: AsyncRead + Unpin + Send> {
    reader: R,
}
impl<R: AsyncRead + Unpin + Send> StreamFrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader
        }
    }
    async fn get_byte(&mut self) -> crate::Result<u8> {
        let mut buff = [0u8; 1];
        let n = self.reader.read(&mut buff[..]).await?;
        if n == 0 {
            Err("Unexpected end of stream".into())
        } else {
            Ok(buff[0])
        }
    }
}
#[async_trait]
impl<R: AsyncRead + Unpin + Send> FrameReader for StreamFrameReader<R> {
    async fn receive_frame(&mut self) -> crate::Result<RpcFrame> {
        let mut lendata: Vec<u8> = vec![];
        let frame_len = loop {
            lendata.push(self.get_byte().await?);
            //let mut cursor = Cursor::new(&lendata);
            let mut buffrd = BufReader::new(&lendata[..]);

            let mut rd = ChainPackReader::new(&mut buffrd);
            match rd.read_uint_data() {
                Ok(len) => { break len as usize }
                Err(err) => {
                    let msg = err.to_string();
                    let ReadError{reason, .. } = err;
                    match reason {
                        ReadErrorReason::UnexpectedEndOfStream => { continue }
                        ReadErrorReason::InvalidCharacter => { return Err(msg.into()) }
                    }
                }
            };
        };
        let mut data: Vec<u8> = Vec::with_capacity(frame_len);
        let mut to_read = frame_len;
        while to_read > 0 {
            let mut buff: Vec<u8> = Vec::with_capacity(to_read);
            buff.resize(to_read, 0);
            let n = self.reader.read(&mut buff[..]).await?;
            data.append(&mut buff);
            to_read -= n;
        }
        let protocol = data[0];
        if protocol == Protocol::ChainPack as u8 {
            let mut buffrd = BufReader::new(&data[1..]);
            let mut rd = ChainPackReader::new(&mut buffrd);
            if let Ok(Some(meta)) = rd.try_read_meta() {
                let pos = rd.position() + 1;
                let data: Vec<_> = data.drain(pos .. ).collect();
                let frame = RpcFrame { protocol: Protocol::ChainPack, meta, data };
                log!(target: "RpcMsg", Level::Debug, "R==> {}", &frame);
                return Ok(frame);
            }
        } else if protocol == Protocol::Cpon as u8 {
            let mut buffrd = BufReader::new(&data[1..]);
            let mut rd = crate::CponReader::new(&mut buffrd);
            if let Ok(Some(meta)) = rd.try_read_meta() {
                let pos = rd.position() + 1;
                let data: Vec<_> = data.drain(pos .. ).collect();
                let frame = RpcFrame { protocol: Protocol::Cpon, meta, data };
                log!(target: "RpcMsg", Level::Debug, "R==> {}", &frame);
                return Ok(frame);
            }
        } else {
            return Err("Unknown protocol".into());
        }
        return Err("Meta data read error".into());
        }
}

pub struct StreamFrameWriter<W: AsyncWrite + Unpin + Send> {
    writer: W,
}
impl<W: AsyncWrite + Unpin + Send> StreamFrameWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
        }
    }
}
#[async_trait]
impl<W: AsyncWrite + Unpin + Send> FrameWriter for StreamFrameWriter<W> {
    async fn send_frame(&mut self, frame: RpcFrame) -> crate::Result<()> {
        log!(target: "RpcMsg", Level::Debug, "S<== {}", &frame.to_rpcmesage().unwrap_or_default());
        let meta_data = serialize_meta(&frame)?;
        let mut header = Vec::new();
        let mut wr = ChainPackWriter::new(&mut header);
        let msg_len = 1 + meta_data.len() + frame.data.len();
        wr.write_uint_data(msg_len as u64)?;
        header.push(frame.protocol as u8);
        self.writer.write(&header).await?;
        self.writer.write(&meta_data).await?;
        self.writer.write(&frame.data).await?;
        // Ensure the encoded frame is written to the socket. The calls above
        // are to the buffered stream and writes. Calling `flush` writes the
        // remaining contents of the buffer to the socket.
        self.writer.flush().await?;
        Ok(())
    }
}


