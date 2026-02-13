//! Protobuf 编解码 + Connect-RPC 帧处理
//! 
//! 手写实现，精确匹配 Windsurf 线上格式。
//! 移植自 Node.js 版本的 protobuf.mjs

use bytes::{BufMut, BytesMut};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use std::io::{Read, Write};

/// Protobuf 编码器
pub struct ProtobufEncoder {
    buf: BytesMut,
}

impl ProtobufEncoder {
    pub fn new() -> Self {
        Self { buf: BytesMut::new() }
    }

    fn write_varint_raw(buf: &mut BytesMut, mut value: u64) {
        loop {
            if value <= 0x7f {
                buf.put_u8(value as u8);
                break;
            }
            buf.put_u8((value as u8 & 0x7f) | 0x80);
            value >>= 7;
        }
    }

    fn write_tag(&mut self, field: u32, wire: u32) {
        Self::write_varint_raw(&mut self.buf, ((field << 3) | wire) as u64);
    }

    /// 写入 varint 字段
    pub fn write_varint(&mut self, field: u32, value: u64) -> &mut Self {
        self.write_tag(field, 0);
        Self::write_varint_raw(&mut self.buf, value);
        self
    }

    /// 写入字符串字段
    pub fn write_string(&mut self, field: u32, value: &str) -> &mut Self {
        let data = value.as_bytes();
        self.write_tag(field, 2);
        Self::write_varint_raw(&mut self.buf, data.len() as u64);
        self.buf.extend_from_slice(data);
        self
    }

    /// 写入 bytes 字段
    pub fn write_bytes(&mut self, field: u32, value: &[u8]) -> &mut Self {
        self.write_tag(field, 2);
        Self::write_varint_raw(&mut self.buf, value.len() as u64);
        self.buf.extend_from_slice(value);
        self
    }

    /// 写入嵌套 message 字段
    pub fn write_message(&mut self, field: u32, sub: &ProtobufEncoder) -> &mut Self {
        let data = sub.as_bytes();
        self.write_tag(field, 2);
        Self::write_varint_raw(&mut self.buf, data.len() as u64);
        self.buf.extend_from_slice(data);
        self
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.buf.to_vec()
    }
}

/// 从 protobuf 数据中提取所有 UTF-8 字符串（长度 > 5）
pub fn extract_strings(data: &[u8]) -> Vec<String> {
    let mut strings = Vec::new();
    let mut i = 0;

    while i < data.len() {
        // Read tag varint
        let (tag, new_i) = decode_varint(data, i);
        if new_i == i { break; }
        i = new_i;

        let wire = tag & 0x7;
        match wire {
            0 => {
                // Varint — skip
                let (_, new_i) = decode_varint(data, i);
                i = new_i;
            }
            1 => { i += 8; } // 64-bit fixed
            2 => {
                // Length-delimited
                let (length, new_i) = decode_varint(data, i);
                i = new_i;
                let length = length as usize;
                if i + length <= data.len() {
                    if let Ok(text) = std::str::from_utf8(&data[i..i + length]) {
                        if text.len() > 5 {
                            strings.push(text.to_string());
                        }
                    }
                }
                i += length;
            }
            5 => { i += 4; } // 32-bit fixed
            _ => break,
        }
    }

    strings
}

/// 解码 varint
pub fn decode_varint(data: &[u8], mut offset: usize) -> (u64, usize) {
    let mut value: u64 = 0;
    let mut shift = 0;
    while offset < data.len() {
        let b = data[offset];
        offset += 1;
        value |= ((b & 0x7f) as u64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            break;
        }
    }
    (value, offset)
}

/// Connect-RPC 帧编码（gzip 压缩）
pub fn connect_frame_encode(proto_bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(proto_bytes).unwrap();
    let compressed = encoder.finish().unwrap();

    let mut frame = Vec::with_capacity(5 + compressed.len());
    frame.push(1); // flags: gzip
    let len = compressed.len() as u32;
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&compressed);
    frame
}

/// Connect-RPC 帧解码
pub fn connect_frame_decode(data: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut i = 0;

    while i + 5 <= data.len() {
        let flags = data[i];
        let length = u32::from_be_bytes([data[i + 1], data[i + 2], data[i + 3], data[i + 4]]) as usize;
        i += 5;

        if i + length > data.len() { break; }
        let payload = &data[i..i + length];
        i += length;

        let decoded = if flags == 1 || flags == 3 {
            // gzip compressed
            let mut decoder = GzDecoder::new(payload);
            let mut buf = Vec::new();
            match decoder.read_to_end(&mut buf) {
                Ok(_) => buf,
                Err(_) => payload.to_vec(),
            }
        } else {
            payload.to_vec()
        };

        frames.push(decoded);
    }

    frames
}

/// gzip 压缩
pub fn gzip_compress(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}
