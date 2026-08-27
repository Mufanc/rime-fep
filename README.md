# rime-fep

基于 Rime 的终端中文输入法前端。`rime-fep` 在 PTY 中运行 shell 或其他命令，并在光标附近显示输入状态和候选词，适用于本地终端和 SSH。

![rime-fep](assets/screenshot.png)

## 构建

需要 Rust、C 编译器，以及 librime 和 OpenCC。macOS 可通过 Homebrew 安装运行库：

```sh
brew install librime
git submodule update --init --recursive
cargo build --release
```

librime 位于非标准路径时，可设置 `RIME_INCLUDE_DIR` 和 `RIME_LIB_DIR`。

## 使用

```sh
# 启动 $SHELL
target/release/rime-fep

# 启动指定命令
target/release/rime-fep -- zellij
target/release/rime-fep -- /bin/zsh -l
```

| 按键 | 功能 |
| --- | --- |
| `Ctrl+\` | 开启或关闭输入法 |
| `` Ctrl+` `` | 打开 Rime 方案选单 |
| 数字键 | 选择候选词或选单项 |
| `Esc` | 取消当前输入 |

## 配置

默认使用 Rime 官方的朙月拼音配置。也可以指定一份未编译的 Rime 配置目录：

```sh
target/release/rime-fep --config-dir ~/.config/rime
```

指定目录只读；配置会与内置文件合并后编译到系统缓存目录，原目录不会被修改。

## 限制

- 输入学习记录目前不会跨进程保留。
- 在传统终端协议下，`Ctrl+Space`、`Ctrl+@` 和 `` Ctrl+` `` 无法区分。
