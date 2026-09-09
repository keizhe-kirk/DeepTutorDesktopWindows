//! COM / Windows IPC 桥接(占位)。
//! M1 实现腾讯 IMA 桌面客户端 COM 接口集成(如可获取到协议元数据)。
//! M0 留空。

#![allow(dead_code)]

#[allow(non_snake_case)]
pub mod Win32 {
    // M1 用 windows crate 引入 CoInitialize / CoCreateInstance
}
