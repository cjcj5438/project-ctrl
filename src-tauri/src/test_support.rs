//! 测试间的共享前置：会占用真实端口的测试必须串行，否则会互相「顺延」到对方的端口，
//! 使 `wait_listening` 观察到错误的端口而产生偶发失败。

#![cfg(test)]

use std::sync::{Mutex, MutexGuard};

/// 任何会 spawn 监听进程 / 占端口的测试都要持有它。
static PORT_LOCK: Mutex<()> = Mutex::new(());

pub fn exclusive_ports() -> MutexGuard<'static, ()> {
    PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
