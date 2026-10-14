# project-ctrl

本机自用的 macOS 桌面项目管理器：收集散落各目录的项目，归类（tag），一键进入。

里程碑 1：脚手架 + SQLite + 添加项目（扫描并勾选 / 添加单个）+ 列表页 +
tag（手动 tag 打/摘/改名/删除、自动 tag、筛选、搜索、排序）+ 路径丢失标记及重新关联/移除。

## 环境

- pnpm 11、node 22、rust 1.99、Xcode CLT
- `@tauri-apps/cli` 为项目 devDependency（不要全局安装）

## 命令

```sh
pnpm install
pnpm tauri dev     # 开发运行
pnpm build         # 前端类型检查 + 构建
cargo test         # Rust 单元测试（在 src-tauri/ 下运行）
```

数据目录默认 `~/.project-ctrl/`，可用环境变量 `PROJECT_CTRL_DATA_DIR` 覆盖
（测试/验证时指向 /tmp 临时目录，避免污染真实数据）。

## 约束

- 对用户项目目录只写 README.md（里程碑 3 才涉及），绝不在项目目录创建其他文件；
  git 全程只读，绝不 commit/add/pull/push/fetch。
- 不要 `git push`。
