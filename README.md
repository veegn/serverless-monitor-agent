# serverless-monitor-agent

`serverless-monitor-agent` 是探针 Agent（运行服务名与二进制为 `sma`），定时采集服务器资源、网络流量和探测数据，并上报到指定的 Worker 地址。

## 快速安装

从面板或后台获取以下 3 个参数：

- `SERVER_ID`：服务器 ID
- `SECRET`：服务器密钥
- `WORKER_URL`：Worker 上报地址，例如 `https://example.com/update`

Linux、OpenWrt、Synology DSM、FreeBSD、macOS 可使用安装脚本自动下载当前系统对应的最新 release：

```bash
curl -fsSL https://raw.githubusercontent.com/veegn/serverless-monitor-agent/main/install.sh | sh -s -- install -id=SERVER_ID -secret=SECRET -url=WORKER_URL
```

如果系统没有 `curl`，可使用 `wget`：

```bash
wget -O- https://raw.githubusercontent.com/veegn/serverless-monitor-agent/main/install.sh | sh -s -- install -id=SERVER_ID -secret=SECRET -url=WORKER_URL
```

### 普通用户安装（非 root）

支持 `systemd --user` 的 Linux 可使用非 root 安装；执行安装时会使用当前用户，不会新建用户；二进制、配置和流量文件会写入 `~/.sma/`，自启动使用 `systemd --user`。Synology DSM、OpenWrt、Alpine/OpenRC 以及其他不支持 `systemd --user` 的系统请使用 root 权限安装。部分 Linux 系统从旧的 root 版切换到非 root 安装时，需要先在 root 下卸载旧版 agent.

## 服务发现自动注册安装（推荐，仅需 Token）

服务端在「设置 → 站点设置」开启**自动发现**后，注册探针仅需要服务端授权 Token 与 Worker URL 即可快速纳管 VPS：
- 省略 `-id`：由服务端在首次上报时自动创建服务器并分配 ID。
- 省略 `-name`：自动获取本机操作系统主机名（`hostname`）作为服务器名称。
- 支持断网故障容灾与本地缓存，详细交互协议见 [协议规范文档](docs/PROTOCOL.md)。

Linux、OpenWrt、Synology DSM、FreeBSD、macOS：

```bash
curl -fsSL https://raw.githubusercontent.com/veegn/serverless-monitor-agent/main/install.sh | sh -s -- install -token=TOKEN -url=WORKER_URL
```

若需自定义服务器名称：

```bash
curl -fsSL https://raw.githubusercontent.com/veegn/serverless-monitor-agent/main/install.sh | sh -s -- install -token=TOKEN -url=WORKER_URL -name=my-server
```

Windows（管理员 PowerShell）：

```powershell
$script = "$env:TEMP\install-sma.ps1"
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/veegn/serverless-monitor-agent/main/install.ps1" -OutFile $script -UseBasicParsing
PowerShell -ExecutionPolicy Bypass -File $script install -token=TOKEN -url=WORKER_URL
```

不传 `-id` 时，Agent 启动后首先通过 HTTP POST 完成服务发现注册：由服务端创建服务器并返回分配的 ID，Agent 立即写入配置文件（`SERVER_ID`），随后切换为 WSS 全双工长连接进行实时监控上报。建号成功的日志为「auto discovery created server successfully」，约 2 分钟后面板才会显示该服务器。若写入配置失败，ID 仍会在本次运行内生效，Agent 会在后续上报中持续重试写入；若始终写不进去，重启后会重新走建号流程——同名会中止退出，名称变化（例如公网 IP 改变）则会建出一台重复服务器，请及时修复配置目录权限。服务端已存在同名服务器（`reused`）、建号失败（`failed`），或服务端未开启自动发现（空 `id` 上报只收到普通 404）时，Agent 只输出一条提示便直接退出：既不复用同名服务器、也不重试，磁盘上不会留下任何标记文件；服务管理器的重启策略已改为仅在异常退出时重启，因此不会被自动拉起（`systemctl status sma` 显示为已停止）。修复后重新执行 `install` 并传入 `-id` 即可恢复。

## Windows 安装

请使用管理员权限打开 PowerShell，然后执行：

```powershell
$script = "$env:TEMP\install-sma.ps1"
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/veegn/serverless-monitor-agent/main/install.ps1" -OutFile $script -UseBasicParsing
PowerShell -ExecutionPolicy Bypass -File $script install -id=SERVER_ID -secret=SECRET -url=WORKER_URL
```

Windows 上如果安装时开启 `-auto_update=1`，自动更新会下载并执行 `C:\ProgramData\sma\sma-update.exe`，部分杀毒软件或安全策略可能拦截。被拦截时请将 `C:\Program Files\sma\sma.exe`、`C:\ProgramData\sma\sma-update.exe`、`C:\ProgramData\sma\sma.cmd` 加入白名单；如环境不允许放行，建议保持 `-auto_update=0`，需要升级时手动指定版本安装。

## 指定版本或 GitHub 代理

默认安装最新 release。需要指定版本时：

```bash
curl -fsSL https://raw.githubusercontent.com/veegn/serverless-monitor-agent/main/install.sh | sh -s -- install --install-version=v1.0.10 -id=SERVER_ID -secret=SECRET -url=WORKER_URL
```

GitHub 下载较慢时，可以配置代理前缀：

```bash
curl -fsSL https://raw.githubusercontent.com/veegn/serverless-monitor-agent/main/install.sh | sh -s -- install --install-ghproxy=https://gh-proxy.example.com -id=SERVER_ID -secret=SECRET -url=WORKER_URL
```

## 常用安装参数

| 参数 | 说明 | 默认值 |
| --- | --- | --- |
| `-id=SERVER_ID` | 服务器 ID，首次安装必填；服务端开启自动发现时可省略 | 无 |
| `-name=NAME` | 服务器名称，未传时自动使用操作系统主机名 (`hostname`) | `hostname` |
| `-token=TOKEN` | 服务端授权 Token，首次安装必填（等价于 `-secret`） | 无 |
| `-secret=SECRET` | 服务器密钥 / 授权 Token，首次安装必填 | 无 |
| `-url=WORKER_URL` | Worker 上报地址，首次安装必填 | 无 |
| `-interval=N` | 上报间隔，单位秒 | `60` |
| `-collect_interval=N` | 采样间隔，单位秒；WSS auto 开启时为 `0` 或大于 `WSS_REPORT_INTERVAL` 会按 `WSS_REPORT_INTERVAL` 采样 | `0` |
| `-ct=HOST` | 电信测试节点，可写 `host` 或 `host:port` | 空 |
| `-cu=HOST` | 联通测试节点，可写 `host` 或 `host:port` | 空 |
| `-cm=HOST` | 移动测试节点，可写 `host` 或 `host:port` | 空 |
| `-bgp=HOST` | BGP 节点测试节点，可写 `host` 或 `host:port` | 空 |
| `-interface=IFACES` | 指定统计网卡，多个用英文逗号分隔 | 自动汇总 |
| `-reset_day=N` | 每月流量重置日，`1-31`；`0` 表示不重置 | `1` |
| `-connection_mode=auto\|http` | 连接模式；默认 `auto` 使用 WSS 实时上报（离线自动落盘并批量补报），保留兼容性选项 | `auto` |
| `-ping_mode=tcp\|icmp` | Ping 探测模式；`tcp` 使用 TCP 连接探测，`icmp` 使用 ICMP 探测；两者均每 20 秒采样一次并按 2 分钟窗口聚合 | `tcp` |
| `-auto_update=0\|1` | 是否开启自动检查更新 | `0` |
| `-rx_correction=N` | 下行流量校正，单位 GB | 空 |
| `-tx_correction=N` | 上行流量校正，单位 GB | 空 |
| `-debug=0\|1` | 是否开启调试日志 | `0` |

再次执行 `install` 时，如果本机已有配置文件，程序会以已有配置为基础，只覆盖本次显式传入的参数，未传入的参数保留旧值（例如只传 `-auto_update=1` 就只修改自动更新开关）。首次安装（本机无已有配置）时 `-secret`、`-url` 必填；`-id` 在服务端开启自动发现时可省略，具体见[自动发现安装](#自动发现安装不传--id)。

自动更新默认关闭。安装时传入 `-auto_update=1` 后，Agent 启动时会检查 GitHub release，之后每 6 小时检查一次；稳定版按版本号更新，`Snapshot-` 版本会跟随最新可用 Snapshot prerelease。自动更新只由本地 `AUTO_UPDATE` 配置控制，不依赖面板返回 `update=1`。如果安装时配置了 `--install-ghproxy`，代理会写入本地配置并用于后续自动更新；该本地字段不参与远端配置 MD5 对比。自动更新的检查结果、调度结果和失败原因始终以 info 级日志输出，无需开启 debug；系统级 nohup 环境下安装过程的输出会追加到 `/var/log/sma.log`，systemd 环境可通过 `journalctl -u 'sma-auto-update-*'` 查看。

自动更新要求目标 release 提供 `checksums.txt`，且其中包含对应资产的 SHA-256；校验文件不可用、校验不符、下载为空或超出 512 MiB 时会停止本次更新，继续运行当前版本。Linux ARM32 按当前二进制构建时的 `GOARM` 选择 `armv5`、`armv6` 或 `armv7` 资产；无法确定兼容级别时跳过自动更新。ARMv6/v7 的软浮点构建也会跳过自动更新，避免替换为需要硬件浮点的发布资产。

普通用户安装时，自动更新会在 `~/.sma/` 内下载并替换当前用户的二进制，随后退出当前进程，由 `systemd --user` 或 macOS LaunchAgent 按服务重启策略拉起新版本；不会触碰 root 路径或系统级服务。

更新检查（`api.github.com`）、新二进制下载（`github.com`）、指标上报以及公网 IP 查询的 DNS 解析默认使用系统原生 DNS；仅在配置了 `--install-ghproxy`（通常为系统 DNS 被污染的国内服务器）或通过环境变量 `CF_PROBE_UPDATE_DNS` 显式指定 DNS 服务器时，才启用 Agent 内置的公共 DNS 轮询解析（阿里、DNSPod、114、Cloudflare、Google，UDP 53），此时不依赖系统 DNS，内置 DNS 全部不可用时回退系统 DNS。`CF_PROBE_UPDATE_DNS` 会自动补全 `:53`端口（如 `CF_PROBE_UPDATE_DNS=223.5.5.5`）。root/system 模式更新时 Agent 会把新二进制下载到配置目录，再调度新二进制执行自身的 `install` 完成替换并重启服务；普通用户模式按上一段直接自替换。两种模式都不经过 install.sh 和 curl；下载或调度失败时旧版本不受影响，下次检查自动重试。如 `github.com` 完全不可达，可配置 `--install-ghproxy` 代理（仅用于二进制文件下载；`api.github.com` 的版本检查始终直连，gh-proxy 类服务不支持 API 转发）。

## 支持的架构

安装脚本根据 `uname -m` 选择对应的 release 资产（资产名形如 `sma-linux-<后缀>`）：

| `uname -m` | 资产后缀 |
| --- | --- |
| `x86_64` / `amd64` | `amd64` |
| `aarch64` / `arm64` | `arm64` |
| `i386` / `i686` | `386` |
| `armv5*` / `armv6*` / `armv7*` / `armv8l` | `armv5` / `armv6` / `armv7`；FreeBSD 统一为 `arm` |
| `mips*` / `mipsel` / `lmips`（32 位） | `mips`（大端） / `mipsle`（小端），字节序见下 |
| `mips64*` / `mips64el`（64 位） | `mips64`（大端） / `mips64le`（小端），字节序见下 |
| `loongarch64` | `loong64` |

MIPS 的字节序**不能**靠 `uname -m` 判断：现代 OpenWrt 内核在大端（ath79/ar71xx）和小端（ramips MT762x）设备上都返回 `mips`。安装脚本改为读取设备自身 shell 这个原生 ELF 的 `e_ident[EI_DATA]` 字节来确定字节序（探测目标不是刚下载的二进制，因为它可能正是错的那一个），据此在 `mips`/`mipsle`（及 `mips64`/`mips64le`）间选择；无法探测时按小端处理（OpenWrt 最常见的情形）。若选错字节序，内核会拒绝执行、BusyBox `sh` 会退回来把二进制当脚本报 `syntax error: unexpected "("`。

MIPS（常见于 OpenWrt 路由器）资产统一使用 `GOMIPS=softfloat` 构建：softfloat 二进制不依赖 CPU 的浮点单元，在没有 FPU 的路由器上同样可以运行，在 hardfloat 用户态上也能正常工作。`uname -m` 无法区分浮点 ABI，因此只发布这一种变体；MIPS 及其他非主流架构的 CPU 使用率使用 `/proc/stat` 采样。

未列出的架构（如 `riscv64`、`s390x`、`ppc64le`）没有发布资产，安装脚本会直接报 `unsupported architecture: <arch>`，请按 [从源码构建](#从源码构建) 自行编译。

## 安装位置

| 系统 | 二进制默认位置 | 配置文件 | 日志 |
| --- | --- | --- | --- |
| Linux non-root (`systemd --user`) | `~/.sma/bin/sma` | `~/.sma/config.conf` | `journalctl --user -u sma -f` |
| Linux / Synology DSM | `/usr/local/bin/sma` | `/etc/config/sma/config.conf` | `journalctl -u sma -f` |
| OpenWrt | `/usr/bin/sma` | `/etc/config/sma/config.conf` | `/var/log/sma.log` |
| FreeBSD | `/usr/local/bin/sma` | `/etc/config/sma/config.conf` | `/var/log/sma.log` |
| macOS (`LaunchAgent`) | `~/.sma/bin/sma` | `~/.sma/config.conf` | `~/.sma/sma.log` |
| Windows | `C:\Program Files\sma\sma.exe` | `C:\ProgramData\sma\config.conf` | `C:\ProgramData\sma\sma.log` |

服务名固定为 `sma`。服务会根据系统自动注册为 `systemd`、`OpenRC`、`procd`、`launchd`、Synology rc、Windows 计划任务，或在不支持服务管理器的环境中后台运行。

Linux 普通用户安装固定使用 `systemd --user`；root 安装仍使用系统级服务。macOS 固定使用当前用户 LaunchAgent，不再安装 root/system 版。普通用户安装会阻止与其他用户或 root 版实例重复运行，但允许覆盖安装当前用户自己的运行实例。

## 查看状态和日志

Linux non-root (`systemd --user`)：

```bash
systemctl --user status sma
journalctl --user -u sma -f
```

systemd 系统：

```bash
sudo systemctl status sma
sudo journalctl -u sma -f
```

OpenRC（Alpine 等）：

```bash
rc-service sma status
tail -f /var/log/sma.log
```

OpenWrt：

```bash
/etc/init.d/sma status
logread -f
```

FreeBSD：

```bash
ps aux | grep sma
tail -f /var/log/sma.log
```

macOS：

```bash
launchctl print gui/$(id -u)/com.serverless.sma
tail -f ~/.sma/sma.log
```

Windows：

```powershell
Get-ScheduledTask -TaskName sma
Get-Content "C:\ProgramData\sma\sma.log" -Wait
```

## 卸载

推荐使用安装脚本触发卸载。脚本会下载临时 `sma` 执行卸载，避免 Windows 下运行中的已安装程序无法删除自身。

Linux、OpenWrt、Synology DSM、FreeBSD、macOS：

```bash
curl -fsSL https://raw.githubusercontent.com/veegn/serverless-monitor-agent/main/install.sh | sh -s -- uninstall
```

普通用户执行卸载只清理当前用户的 `~/.sma/`、支持 `systemd --user` 的 Linux 用户服务或 macOS LaunchAgent 自启动项；Synology DSM、OpenWrt、Alpine/OpenRC 以及其他系统级安装请使用 root 权限卸载。root 执行卸载清理系统级安装。macOS root 卸载会同时清理 sudo 调用者对应的用户版安装；旧 root/system 版请先执行 `sudo /usr/local/bin/sma uninstall` 清理，再以普通用户安装。

Windows 请使用管理员权限打开 PowerShell：

```powershell
$script = "$env:TEMP\install-sma.ps1"
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/veegn/serverless-monitor-agent/main/install.ps1" -OutFile $script -UseBasicParsing
PowerShell -ExecutionPolicy Bypass -File $script uninstall
```

卸载会清理当前默认安装创建的固定位置和自启动项，不处理旧脚本或手动放置到其他路径的文件。

## 上报数据说明

已绑定节点通过 WebSocket 上报。`WORKER_URL` 填写 HTTP(S) 上报地址，例如 `https://example.com/update`；Agent 会将其转换为 `wss://example.com/update` 并进行标准 WebSocket 握手。POST 用于自动注册及流量修正确认；断线时继续采样、缓存并重连。

服务端 Hello 指定初始模式：

```json
{ "type": "hello", "protocol": "update", "realtimeHint": false, "nextWssReportAfterMs": 60000, "modeRevision": 1 }
```

Agent 在实时、节能和断线状态下都每 2 秒独立采样并写入本地历史队列。实时快照按服务端协商周期发送，支持 1 秒周期；无访客时通常每 60 秒加减 3 秒抖动发送历史批次。实时快照与历史批次独立调度，历史 ACK 延迟不会阻塞看板刷新。系统动态指标现场采集，静态系统身份缓存；可选 GPU 工具在独立线程按 30 秒周期采集。月流量在内存逐次累加、每 30 秒写入检查点，异常退出可能丢失尚未保存的增量。

模式推送和报告确认分别使用 `ackFor:"mode"` 和 `ackFor:"report"`：

```json
{ "type": "ack", "ackFor": "mode", "modeRevision": 2, "realtimeHint": true, "nextWssReportAfterMs": 1000 }
```

```json
{ "type": "ack", "ackFor": "report", "reportId": "b-example", "accepted": true, "persisted": true, "acceptedSamples": 30 }
```

模式推送只接受递增的 `modeRevision`。历史批次只有收到匹配 `reportId`、完整样本数和持久化确认后才清理；重试及重启恢复保持原报文和 ID。实时快照允许由更新的快照替代，历史数据已独立保存在队列中。服务端必须按观测时间更新最新状态，防止较旧批次覆盖较新快照。

默认历史队列上限为 5000 条，按 2 秒采样约覆盖 2.78 小时，容量超限时淘汰最老记录。历史指标包含磁盘 IO、负载、连接数和探测 RTT/丢包率。WebSocket 采用单线程读写及有界发送队列，底层 Ping 周期为 30 秒。HTTP 401/403 停止 WSS 重试并报告凭据错误；Close 1013 按 5、10、30、60 秒递增退避；Close 1009 缩小批次并使用新 ID。完整报文、持久化及服务端要求见 [自适应上报规范](docs/ADAPTIVE_REPORTING_SPEC.md)。

WSS 也支持服务端下发动态配置，配置内容复用旧 POST 响应里的 query-string body；Agent 侧最短每 1 分钟处理一次 WSS 配置下发，服务端不需要在每个 ack 中都携带配置。`configMd5` 可选，带上时会按旧 `X-Agent-Config-Md5` 逻辑更新本地配置版本；不带时 Agent 会按配置字段是否变化决定是否写入本地配置。

```json
{
  "type": "config",
  "body": "collect_interval=2&report_interval=60&reset_day=1&schema_version=6&interface=&connection_mode=auto&wss_report_interval=2&ping_mode=tcp"
}
```

也可以把配置放在 `payload` 对象中：

```json
{
  "type": "config",
  "payload": {
    "collect_interval": 2,
    "report_interval": 60,
    "reset_day": 1,
    "schema_version": 6,
    "interface": "",
    "connection_mode": "auto",
    "wss_report_interval": 2,
    "ping_mode": "tcp"
  }
}
```

完整上报结构如下：

```json
{
  "id": "SERVER_ID",
  "secret": "SECRET",
  "time": {
    "local_ts": 1754300060123,
    "accurate_ts": 1754300060000,
    "offset_ms": -123,
    "source": "date",
    "round_trip_ms": 18,
    "sample_age_ms": 29982
  },
  "metrics": {
    "cpu": "0.00",
    "ram_total": "0",
    "ram_used": "0",
    "swap_total": "0",
    "swap_used": "0",
    "disk_total": "0",
    "disk_used": "0",
    "disk": {
      "read_bps": 0,
      "write_bps": 0,
      "read_iops": 0,
      "write_iops": 0,
      "await_ms": 0,
      "util": 0
    },
    "load_avg": "0 0 0",
    "boot_time": "0",
    "net_rx": "0",
    "net_tx": "0",
    "net_rx_monthly": "0",
    "net_tx_monthly": "0",
    "net_in_speed": "0",
    "net_out_speed": "0",
    "os": "Linux",
    "arch": "amd64",
    "kernel_version": "",
    "cpu_info": "",
    "cpu_cores": "0",
    "gpu_info": null,
    "processes": "0",
    "tcp_conn": "0",
    "udp_conn": "0",
    "ip_v4": "0",
    "ip_v6": "0",
    "ping_ct": false,
    "ping_cu": false,
    "ping_cm": false,
    "ping_bd": false,
    "loss_ct": false,
    "loss_cu": false,
    "loss_cm": false,
    "loss_bd": false
  },
  "collect_interval": 0,
  "report_interval": 60
}
```

当 `COLLECT_INTERVAL > 0` 时，上报体会额外包含 `samples`。`samples` 中只包含高频采样需要的轻量字段，不包含磁盘 IO：

```json
{
  "samples": [
    {
      "ts": 1720000000000,
      "metrics": {
        "cpu": "0.00",
        "ram_total": "0",
        "ram_used": "0",
        "swap_total": "0",
        "swap_used": "0",
        "net_in_speed": "0",
        "net_out_speed": "0"
      }
    }
  ]
}
```

顶层字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `id` | string | 服务器 ID，对应本地 `SERVER_ID` |
| `secret` | string | 服务器密钥，对应本地 `SECRET` |
| `time` | object | 本机墙钟与独立时间校准状态；字段见下表 |
| `metrics` | object | 当前上报周期的完整监控指标 |
| `samples` | array | 可选，仅 `COLLECT_INTERVAL > 0` 时存在 |
| `collect_interval` | number | 高频采样间隔，单位秒；`0` 表示不启用高频采样 |
| `report_interval` | number | 上报间隔，单位秒 |
| `config_schema` | string | 动态配置协议版本，当前为 `6`；WSS 建连 query、首次上报、约每 60 秒或 MD5 变化时携带 |
| `config_md5` | string | 本地保存的远端配置 MD5；首次或为空时为 `none`，WSS 建连 query 与消息字段携带，POST 保留 `X-Agent-Config-Md5` Header |

`time` 字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `local_ts` | number | 本次组包时的本机 Unix 毫秒墙钟 |
| `accurate_ts` | number/null | 根据最近一次 Worker 响应头 `Date` 时间样本，以单调时钟推算的 Unix 毫秒时间；首次校准前为 `null` |
| `offset_ms` | number/null | `accurate_ts - local_ts`；正数表示本机慢，负数表示本机快 |
| `source` | string/null | 成功校准后为 `date` |
| `round_trip_ms` | number/null | 最近一次成功校准请求的往返耗时 |
| `sample_age_ms` | number/null | 最近校准样本到本次组包的单调时钟年龄 |

Agent 只使用 Worker 成功 HTTP 响应里的 `Date` 头做时间校准，包括 POST 响应和 WSS 握手响应。响应头示例：`Date: Thu, 13 Aug 2026 00:23:22 GMT`。Agent 会按该格式解析为 Unix 毫秒时间戳，并结合本次请求 RTT 锚定到单调时钟。若新 `Date` 样本与当前已校准时间的差值在 20 秒内，则跳过更新；首次校准、校准过期或差值超过 20 秒时才覆盖。校准样本最长使用 24 小时。上报前会用同一锚点换算 `samples[].ts`，并校正 `metrics.boot_time`。Agent 不修改系统时间。

`metrics` 字段：

| 字段 | 类型 | 单位/取值 | 说明 |
| --- | --- | --- | --- |
| `cpu` | string | `%` | CPU 使用率，保留 2 位小数 |
| `ram_total` | string | MiB | 物理内存总量 |
| `ram_used` | string | MiB | 物理内存已用量 |
| `swap_total` | string | MiB | Swap 总量；不支持的平台为 `0` |
| `swap_used` | string | MiB | Swap 已用量；不支持的平台为 `0` |
| `disk_total` | string | MiB | 过滤并去重后的本地磁盘总容量 |
| `disk_used` | string | MiB | 过滤并去重后的本地磁盘已用容量 |
| `disk` | object | 见下表 | 磁盘总 IO 信息，采样频率跟随 `REPORT_INTERVAL` |
| `load_avg` | string | `1m 5m 15m` | 系统负载；Windows 为 `0 0 0` |
| `boot_time` | string | ms | 系统启动时间，Unix 毫秒时间戳 |
| `net_rx` | string | bytes | 当前网卡累计接收字节数 |
| `net_tx` | string | bytes | 当前网卡累计发送字节数 |
| `net_rx_monthly` | string | bytes | 当前统计周期内累计下行流量 |
| `net_tx_monthly` | string | bytes | 当前统计周期内累计上行流量 |
| `net_in_speed` | string | bytes/s | 上报周期内平均下行速度 |
| `net_out_speed` | string | bytes/s | 上报周期内平均上行速度 |
| `os` | string | - | 操作系统名称 |
| `arch` | string | - | CPU 架构 |
| `kernel_version` | string | - | 内核或系统版本 |
| `cpu_info` | string | - | CPU 型号或架构兜底值 |
| `cpu_cores` | string | count | CPU 逻辑核心数 |
| `gpu_info` | array/null | - | GPU 信息数组；不可获取时为 `null` |
| `processes` | string | count | 进程数量 |
| `tcp_conn` | string | count | TCP 已建立连接数 |
| `udp_conn` | string | count | UDP 连接或端点数量 |
| `ip_v4` | string | IP/`0` | Cloudflare trace 获取到的 IPv4 |
| `ip_v6` | string | IP/`0` | Cloudflare trace 获取到的 IPv6 |
| `ping_ct` / `ping_cu` / `ping_cm` / `ping_bgp` | string/boolean | ms/`false`/`"null"` | 电信、联通、移动、BGP 节点探测 RTT；节点未配置为 `false`，探测失败为字符串 `"null"` |
| `loss_ct` / `loss_cu` / `loss_cm` / `loss_bd` | string/boolean | `%`/`false` | 对应探测丢包率；节点未配置为 `false` |

`disk` 字段：

| 字段 | 类型 | 单位 | 说明 |
| --- | --- | --- | --- |
| `read_bps` | number | bytes/s | 上报周期内平均读取速率 |
| `write_bps` | number | bytes/s | 上报周期内平均写入速率 |
| `read_iops` | number | ops/s | 上报周期内平均读 IOPS |
| `write_iops` | number | ops/s | 上报周期内平均写 IOPS |
| `await_ms` | number | ms | 读写请求平均等待时间 |
| `util` | number | `%` | 过滤后磁盘集合的平均繁忙度，范围 `0-100` |

Linux 下磁盘 IO 使用 `/proc/diskstats` 计算，并复用磁盘容量统计的过滤和去重规则；Windows、macOS、FreeBSD 以及其他平台暂不上报复杂磁盘 IO，字段保留为 `0`。

`gpu_info` 数组元素结构如下：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | GPU 名称 |
| `info` | number/null | GPU 使用率或平台兜底值 |
| `id` | string | GPU 序号或平台标识 |
| `mem_used` | number/省略 | 已用显存，MiB |
| `mem_total` | number/省略 | 显存总量，MiB |
| `sm_clock` | number/省略 | SM 频率，MHz |
| `power` | number/省略 | 功耗，W |

`mem_used`、`mem_total`、`sm_clock`、`power` 来自 `nvidia-smi` 详情查询（`memory.used`、`memory.total`、`clocks.sm`、`power.draw`），仅在可获取时上报，单位不支持时为 `[N/A]` 会直接省略该字段；旧驱动不支持上述字段时回退为仅上报使用率。

POST fallback 会携带以下 HTTP 头：

| Header | 说明 |
| --- | --- |
| `Content-Type: application/json` | 请求体格式 |
| `Accept: */*` | 接收任意响应格式 |
| `User-Agent: cfsm` | Agent 标识 |
| `X-Agent-Config-Schema` | 当前配置协议版本 |
| `X-Agent-Version` | 当前 Agent 版本 |
| `X-Agent-Config-Md5` | 本地保存的远端配置 MD5；为空时为 `none` |

WSS 握手会携带标准 WebSocket Upgrade 头，并附带以下 Agent 头：

| Header | 说明 |
| --- | --- |
| `Accept: */*` | 接收任意响应格式 |
| `User-Agent: cfsm` | Agent 标识 |
| `X-Agent-Config-Schema` | 当前配置协议版本 |
| `X-Agent-Version` | 当前 Agent 版本 |
| `X-Agent-Config-Md5` | 本地保存的远端配置 MD5；为空时为 `none` |

当 Worker 下发 `rx_correction` 或 `tx_correction` 后，Agent 会额外发送一次流量校正确认：

```json
{
  "id": "SERVER_ID",
  "secret": "SECRET",
  "rx_correction": 0,
  "tx_correction": 0
}
```

其中 `rx_correction` 和 `tx_correction` 为 number，单位 GB。该确认请求只携带 `Content-Type`、`Accept` 和 `User-Agent` 头。

## GitHub CI 自动构建

本项目通过 GitHub Actions 自动化 CI/CD（`.github/workflows/release.yml` 和 `.github/workflows/snapshot.yml`）提供全平台交叉编译二进制产物：

- **自动化构建目标**：
  - Linux: `x86_64` (amd64), `aarch64` (arm64), `i686` (386), `armv7` 等（基于 musl 静态链接，无需依赖系统 libc）
  - macOS: `x86_64` (Intel), `aarch64` (Apple Silicon)
  - Windows: `x86_64`, `aarch64`, `i686`
  - FreeBSD: `x86_64`
- **校验和保障**：每次构建自动生成 `checksums.txt`（包含各二进制的 SHA-256 校验和），发布至 GitHub Releases，保证更新和安装的完整性与安全性。

## 从源码构建

需要 Rust `1.80` 或更新版本（带有 Cargo）。

```bash
git clone https://github.com/veegn/serverless-monitor-agent.git
cd serverless-monitor-agent
cargo build --release
```

编译产出的二进制文件位于 `target/release/sma`。

构建后可直接安装：

```bash
./target/release/sma install -id=SERVER_ID -secret=SECRET -url=WORKER_URL
```

前台调试运行：

```bash
./target/release/sma run -config ./config.conf -debug=1
```

查看帮助：

```bash
./target/release/sma help
```

## 致谢

- [komari-agent](https://github.com/komari-monitor/komari-agent)：本项目的部分监控指标统计口径参考了该项目的实现。
