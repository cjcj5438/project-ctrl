# project-ctrl

本机自用的 macOS 桌面项目管理器：把散落在各目录里的项目收集起来、打 tag 归类，
一键用 Finder / iTerm / VS Code 打开，并查看 README 与只读的 git 状态。

## 功能

- **添加项目**：「扫描文件夹…」选择目录后列出其一级子目录（跳过隐藏目录，已加入的不再出现），
  勾选后可批量打 tag 再「加入」；「添加单个项目…」把选中的目录本身作为项目加入。候选项目不要求有 `.git`。
- **列表页**：卡片网格（项目名＝文件夹名、路径、创建日期、tag）；顶部名称搜索、tag 多选筛选（AND 语义）、
  排序切换（创建时间倒序 / 名称）。手动 tag 可在筛选栏改名 / 删除（删除前确认，并从所有项目摘掉）。
- **自动 tag**（不入库、不可手改，也可用于筛选）：`git`（存在 `.git`）、`无git`、`github`（`.git` 为目录且 remote 含 github.com）。
- **详情页**：点卡片进入（返回后保留搜索 / 筛选 / 排序）。可增删手动 tag；Finder / iTerm / VS Code /
  （识别出 GitHub remote 时）GitHub 按钮；「从管理器移除」只删记录、不动文件。
- **README**：渲染（marked 转 HTML 后经 DOMPurify 清洗）、编辑（CodeMirror 6，左编辑右实时预览，
  `Cmd+S` 保存；编辑期间文件被外部改动会提示冲突，不静默覆盖）、无 README 时按模板生成。
- **只读 git**：存在 `.git` 的项目可查看 `git status` / `git log`，输出显示在详情页面板里（等宽、可滚动、可复制）。
- **项目大小**：卡片与详情页显示项目磁盘占用（如 `12.3 MB`）。目录统计在后台线程进行（并发上限 3），
  列表不会被阻塞；结果只缓存在前端内存（不入库），路径丢失的项目不统计也不显示。
- **路径丢失**（目录被删 / 改名 / 移动）：保留记录，卡片与详情页显示「路径丢失」，提供「重新关联」与「移除」。

## 环境

- macOS（Apple Silicon）、pnpm 11、node 22、Rust stable、Xcode Command Line Tools
- `@tauri-apps/cli` 是项目 devDependency（不要全局安装）

## 开发

```sh
pnpm install
pnpm tauri dev      # 开发运行（同时启动 vite 与 Tauri 应用）
pnpm build          # 前端类型检查 + 构建
cargo test          # Rust 单元测试（在 src-tauri/ 下运行）
```

## 构建 release .app

```sh
pnpm tauri build --bundles app
```

产物在 `src-tauri/target/release/bundle/macos/project-ctrl.app`（arm64，ad-hoc 签名），复制到
`/Applications` 即可使用。`tauri.conf.json` 中 `bundle.macOS.signingIdentity = "-"`：Apple Silicon 上
未签名二进制无法运行，这里用 ad-hoc 签名，使 `pnpm tauri build` 一步产出可运行的 .app。
不做公证、不做自动更新、不生成 dmg。

本地构建出的 .app 不带 quarantine 标记，通常可直接打开；本项目未做公证，若 .app 是从别处拷贝或下载来的
而遇到「无法验证开发者」，可在「系统设置 → 隐私与安全性」中对该应用选择「仍要打开」。

## 数据与配置

- SQLite 库位于 `~/.project-ctrl/`，只有 3 张表：`projects`（路径唯一、加入时间）、`tags`（名称唯一）、
  `project_tags`（关联，外键级联）。
- 环境变量 `PROJECT_CTRL_DATA_DIR` 可覆盖数据目录（测试 / 验证时指向 `/tmp`，避免污染真实数据）。

## 对用户目录的写入边界

- 只写项目目录里的 README 文件本身（生成 / 保存；不创建临时文件或备份文件），不写任何其他文件。
- git 只执行 `status` 与 `log` 两个只读命令（环境带 `GIT_OPTIONAL_LOCKS=0`，不改写仓库索引），
  绝不执行 add / commit / fetch / pull / push 等写操作。
- 打开外部链接只允许 `http` / `https` / `mailto`。

## 已知局限

- 创建日期读取目录 birthtime，目录被拷贝或备份恢复后会变。
- `.git` 为文件时（linked worktree / submodule）只标记 `git`，不做 GitHub 识别。
- 首次点击 iTerm 按钮时，macOS 会询问是否允许 project-ctrl 控制 iTerm（自动化授权）。
- 项目大小口径为**磁盘占用**（`st_blocks × 512`，与 `du` 同口径）：递归包含 `node_modules` 与 `.git`，
  同一 inode 的硬链接只算一次，不跟随符号链接，读不了的子目录跳过；显示用 1000 进制
  （B / KB / MB / GB / TB，小于 1000 B 显示整数 B，其余保留 1 位小数）。
  APFS 上目录自身在 `st_blocks` 中报 0，因此在极端的海量硬链接场景下与 `du` 的统计可能略有出入。

## 开发提示

调试（debug）构建支持用数据目录下的 `dev-pick-dir` 文件旁路原生选目录弹窗，便于自动化验证；
release 构建恒不生效。
