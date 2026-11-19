//! 还没有后端的平台:核心代码照样能编译,运行时报错

use std::sync::Arc;

use anyhow::{Result, bail};

use crate::host::{Host, Stream, Target};

pub fn init() {}

pub fn preset(name: &str) -> Result<Target> {
    bail!("这个平台还没有内置档案 {name}")
}

pub fn spawn(_target: Target, _stream: Stream) -> Result<Arc<dyn Host>> {
    bail!("这个平台还没有推流后端")
}
