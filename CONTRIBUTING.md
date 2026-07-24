# 贡献指南 Contributing

感谢贡献！

## 开发流程

1. Fork 并 `git clone`。
2. 从 `dev` 分支切出功能分支。
3. 本地验证：`just ci`（等价于 CI：fmt + clippy + test）。
4. 提交 PR 到 `dev`，通过 CI 与 review 后合并。

## 代码风格

- Rust：`cargo fmt`（配置见 `.rustfmt.toml`），`cargo clippy -- -D warnings`。
- Dart：`flutter analyze`。
- 提交信息建议遵循 Conventional Commits。

## 架构约束

- `shared/` 的 crate 被 `core/` 与 `cloud/` 同时依赖，修改需考虑两端。
- FFI 边界唯一：`flutter/` 只允许通过 `rdcore-ffi`（flutter_rust_bridge 生成）调用 Rust。
- 云端只处理控制面元数据，不得引入屏幕/键鼠数据通路。
