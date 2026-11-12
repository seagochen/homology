# AGENTS.md：在 Homology 上工作

Homology 是 AnnotaryCat 和 Crescent 共用的 runner 与远程访问层。它在 AnnotaryCat 中是 submodule `crates/homology`，在 Crescent 中由 `crescent-host` 使用。改动前先读这些规则。

## 1. 独立

- 不依赖 AnnotaryCat、Crescent 或任何界面库（egui 等）。调用方的东西（程序名、版本、缓存目录、自己的操作、自己的检查规则）都由调用方传进来。
- `homology-runner` 要上传到用户的服务器：只依赖 `homology-sys`、`serde_json`、`sha2`、`hex`，用 `scripts/build-runners.sh` 静态构建（musl，rust-lld 链接，`.cargo/config.toml`）。新依赖要说明为什么不能不用。
- crate 之间的依赖只能向下：`homology` → `homology-ssh`、`homology-runner` → `homology-sys`；`homology-ssh` 与 `homology-runner` 互不依赖。两个以上 crate 要用的系统辅助函数放进 `homology-sys`（只用标准库），不复制。

## 2. 协议是稳定 API

- op 名和错误码（`code`）是稳定的：AnnotaryCat 的命令和脚本按它们判断。改名等于破坏兼容；要改，先升 `PROTOCOL`。
- 每个失败都是 `RunnerError { code, message }`；`message` 给人看，`code` 给程序判断。
- 新的通用操作加进 `server::OPS` 并在 `hello` 中列出；只属于某个调用方的操作留在调用方，经 `server::Runner` 加入。

## 3. 数据安全

- 只用系统 `ssh`、用户的 `~/.ssh/config`、agent 和密钥；不保存密码，不把用户的密钥复制到服务器。
- 未知主机的密钥必须经用户确认后才写入 known_hosts。
- runner 不开端口、不常驻、只活一个会话。服务器上只在调用方指定的缓存目录里留东西，缓存里只放可以重新得到的东西。
- 写文件只覆盖调用方打开时的版本（sha256），经临时文件再改名，从不写一半。
- 写文件一律用 `homology_sys::atomic_write_bytes`（或 `temporary_beside`）：临时文件名随机、只在没有任何东西时独占创建（`create_new`），从不打开或跟随目录里已有的文件与符号链接（#3）；不要自己拼 `<名>.tmp`。
- 消息、日志和错误信息里从不出现密码、私钥或令牌。
- 检查：`crates/homology-ssh/tests/secrets.rs` 用假的 `ssh` 走一遍密码登录和登录密钥的设置，确认密码不出现在 ssh 的参数、环境、stdin、任何文件和错误里，私钥不到服务器。改了密码、askpass 或登录密钥的流程时，这个检查要跟着覆盖新的路径。

## 4. 不 panic

工作区 lint 禁止 `unwrap`、`expect`、`panic!`、`todo!`、`unimplemented!`、`unreachable!`（测试除外），并禁止 `unsafe`。唯一的例外是 `homology-winframe`：Windows 窗口消息只能经 Win32 处理，它在 crate 级别改为 `deny`，只在 `ffi` 模块中允许 `unsafe`。返回错误，不要崩溃。

## 5. 风格

- 代码、注释和文档注释用英文；issue、README、本文件用中文。
- 每个函数、类型、常量、模块都要有文档注释，说明它是做什么的、为什么；字段和枚举成员在名字说不清时也要有。
- 命名用完整的词。行宽 120（`rustfmt.toml`）。

## 6. 提交前

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
scripts/build-runners.sh     # 改了 homology-runner 或它的依赖时：两个架构的静态构建
```

`cargo test` 包括属性测试（`crates/homology-runner/src/properties.rs`，proptest）。改了协议解析、路径规则、文件操作或读行时，相应的属性也要跟着改；随机找到的失败先修，再把最小反例写成固定的回归测试。

`fuzz/`（cargo-fuzz，独立 workspace，不进 runner）在改了协议、`serve` 或程序输出的解析之后手动跑：`cargo +nightly fuzz run <target> -- -max_total_time=600`，target 见 `cargo +nightly fuzz list`。`serve` target 会先把 `hello`、`ping`、`shutdown` 以外的请求改成不存在的操作，fuzz 不会动本机的文件；新增 target 时保持这一点。

CI（`.github/workflows/ci.yml`）在 Linux、Windows、macOS 上跑上面前三项，并在 Linux 和 macOS 上跑 `scripts/build-runners.sh`；推送前本机先跑过，CI 不是第一次检查。

在 AnnotaryCat 中改动 Homology 时：先在本仓库提交并推送到 `main`，再在 AnnotaryCat 中提交 submodule 的新提交；两边的测试都要通过。
