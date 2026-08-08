[English](README.md) | [简体中文](README.zh-CN.md)

# doubao-asr-rust

豆包即时语音识别（Doubao IME）的非官方异步 Rust SDK、命令行工具与
OpenAI 兼容 HTTP 服务。

提供商接口未被官方文档化，可能随时变更。需要受支持服务条款的生产环境，
请使用火山引擎官方 API。

## 环境要求

- Rust 1.86 或更高版本
- `ffmpeg` 位于 `PATH` 中，且编译时启用了 `libopus` 编码器
- 可访问豆包设备注册与语音识别端点

## 构建与测试

```bash
cargo build --release
cargo test --all-targets
```

## Docker

预构建的 `linux/amd64` 与 `linux/arm64` 镜像发布在 GitHub Container Registry：

```bash
docker run --rm -p 127.0.0.1:8000:8000 \
  -e DOUBAO_ASR_API_KEY=change-me \
  -v doubao-asr-data:/data \
  ghcr.io/6kmfi6hp/doubao-asr-rust:latest
```

镜像以 UID/GID `10001` 的非 root 用户运行，凭据保存在
`/data/asr_credentials.json`，并内置带 `libopus` 编码器的 FFmpeg。
服务器监听容器内所有网卡接口，而上面的示例只将其发布到宿主机回环地址。

使用加固版 Compose 配置：

```bash
cp .env.example .env
# 需要客户端鉴权时，在 .env 中设置 DOUBAO_ASR_API_KEY
docker compose up -d
docker compose ps
curl --fail http://127.0.0.1:8000/healthz
```

Compose 使用只读根文件系统、受限的临时文件系统处理音频、移除 Linux
capabilities，并将凭据持久化到 `doubao-asr-data` 卷。不丢弃凭据即可升级：

```bash
docker compose pull
docker compose up -d
```

在本地构建相同镜像：

```bash
docker build --build-arg VERSION=dev -t doubao-asr:dev .
```

## 命令行工具

转写本地音视频文件中的第一个音频流：

```bash
./target/release/doubao-asr recording.wav
./target/release/doubao-asr --json --output transcript.json recording.mp3
```

首次成功请求会匿名注册一个豆包 IME 设备，并将其凭据保存到操作系统的用户
配置目录中。Unix 上凭据以仅属主可读（`0600`）的权限原子写入。
提供商会话被拒绝时，会先执行同设备 token 刷新并重试；只有刷新后的 token
仍被拒绝，才会重新注册一次设备。因此 token 过期后可以自动恢复，无需手动
删除凭据文件。

运行 `doubao-asr --help` 查看全部 CLI 选项。

## OpenAI 兼容服务

启动服务：

```bash
./target/release/doubao-asr-server
```

默认监听 `127.0.0.1:8000`，提供：

- `POST /v1/chat/completions`
- `GET /v1/models`
- `GET /v1/models/doubao-asr`
- `GET /healthz`

仅支持语音转文字请求。每个请求必须恰好包含一个音频部分；文本部分仅为
客户端兼容而接受，不会发送给豆包后端。

### OpenAI `input_audio`

`input_audio` 接受 base64 编码的 WAV 或 MP3 数据：

```python
import base64
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8000/v1", api_key="unused")

with open("recording.wav", "rb") as audio_file:
    audio = base64.b64encode(audio_file.read()).decode("ascii")

response = client.chat.completions.create(
    model="doubao-asr",
    messages=[
        {
            "role": "user",
            "content": [
                {
                    "type": "input_audio",
                    "input_audio": {"data": audio, "format": "wav"},
                }
            ],
        }
    ],
)
print(response.choices[0].message.content)
```

### vLLM/Qwen 风格 `audio_url`

`audio_url` 接受公网或私网的 HTTP(S) URL，以及 base64 data URL：

```bash
curl http://127.0.0.1:8000/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "doubao-asr",
    "messages": [{
      "role": "user",
      "content": [{
        "type": "audio_url",
        "audio_url": {"url": "https://example.com/recording.wav"}
      }]
    }]
  }'
```

`file:///tmp/recording.wav` 这类本地文件 URL 会被拒绝。本地文件请使用
`input_audio` 编码，或通过 HTTP(S) 对外提供。

### 延迟流式返回

带 `"stream": true` 的请求会收到合法的 Chat Completions SSE 分片，并以
`data: [DONE]` 结束。豆包识别仍会在第一个分片发送之前完成；这只是协议
兼容，并非实时转写。

## 服务配置

| 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `DOUBAO_ASR_LISTEN` | `127.0.0.1:8000` | 监听地址 |
| `DOUBAO_ASR_API_KEY` | 未设置 | 可选 Bearer API 密钥 |
| `DOUBAO_ASR_CREDENTIALS` | 系统用户配置目录 | 凭据文件路径 |
| `DOUBAO_ASR_MODEL` | `doubao-asr` | 对外公布并接受的模型 ID |
| `DOUBAO_ASR_UPLOAD_SPEED` | `1` | 上传倍速，大于 0 且不超过 4 |
| `DOUBAO_ASR_MAX_CONCURRENCY` | `2` | 同时处理的请求数 |
| `DOUBAO_ASR_REQUEST_TIMEOUT_SECS` | `1800` | 单请求端到端超时 |
| `DOUBAO_ASR_CREDENTIAL_REFRESH_SECS` | `21600` | 同设备主动刷新 token 间隔；`0` 表示关闭 |

对应命令行参数可查看：

```bash
doubao-asr-server --help
```

设置 `DOUBAO_ASR_API_KEY` 后，所有 `/v1/*` 请求都需要携带：

```text
Authorization: Bearer <key>
```

`/healthz` 始终保持免鉴权。

实测匿名 settings 响应不包含 token 过期时间或 TTL，因此服务默认每六个小时
复用同一设备身份刷新一次 token。它每分钟检查一次，刷新失败时保留当前凭据，
并按 1、5、15、60 分钟退避重试。即使关闭了主动刷新，提供商会话被拒绝时
仍会立即触发恢复。

## 限制与安全

- 解码后音频上限 25 MiB；JSON 请求体上限 36 MiB。
- 远程下载 30 秒超时，最多跟随五次重定向。
- 并发占满时按 OpenAI 风格返回 HTTP 429。
- 每个请求完成或取消时都会删除临时音频文件。
- 日志不会记录 API 密钥、提供商 token、音频 URL、请求体、转写文本或音频
  数据。

远程 `audio_url` 请求有意允许访问 localhost、私有网络、链路本地地址等
任意 HTTP(S) 目标，这构成 SSRF 能力。**请勿在未设置强
`DOUBAO_ASR_API_KEY` 以及没有反向代理或防火墙控制的情况下，将服务绑定到
公网网卡。**

## 自动发布

版本根据 Conventional Commit 消息自动准备：`fix:` 提交触发补丁版本，
`feat:` 触发次版本，破坏性变更触发主版本。Release Please 会打开或更新
Release PR；合并该 PR 后创建 GitHub Release 并发布：

- `amd64` 与 `arm64` 的 GHCR 镜像标签 `X.Y.Z`、`X.Y`、`latest`
- `x86_64` 与 `aarch64` 的静态 musl CLI/Server 压缩包
- `SHA256SUMS` 校验文件、镜像 SBOM 与构建来源证明

下载的二进制仍要求宿主机安装带 `libopus` 的 FFmpeg。发布工作流也提供手动
恢复入口，可在附件发布中断时重建某个已存在的 `vX.Y.Z` 版本。

每个 Release 页面都会附带中文版本说明；详细变更记录见英文
[CHANGELOG.md](CHANGELOG.md)。

## Rust SDK

```rust,no_run
use doubao_asr::Client;

#[tokio::main]
async fn main() -> doubao_asr::Result<()> {
    let client = Client::new()?;
    let transcript = client.transcribe_file("recording.wav").await?;
    println!("{}", transcript.text);
    Ok(())
}
```

## 许可证

MIT