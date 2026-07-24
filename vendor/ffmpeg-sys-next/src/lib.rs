#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(clippy::approx_constant)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::redundant_static_lifetimes)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]
// 以下为 bindgen 生成代码在新版 rustc 下的固有噪音（仓库内置补丁时追加）：
// 结构体含函数指针字段并 derive(PartialEq) 触发 fn 地址比较告警；
// 以及按 C ABI 布局生成的指针 transmute 被判 unnecessary。均为生成物风格问题，
// 无运行时影响，静音以免淹没真实告警（每次构建 33 条）。
#![allow(unpredictable_function_pointer_comparisons)]
#![allow(unnecessary_transmutes)]

extern crate libc;

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

#[macro_use]
mod avutil;
pub use avutil::*;
