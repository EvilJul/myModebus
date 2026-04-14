# myModebus

Modbus 协议调试工具，支持 RTU 串口和 TCP 两种通信模式。桌面端应用，基于 Tauri 2 + React + Rust 构建。

## 功能

- RTU / TCP 双模式连接
- 8 种标准功能码读写（0x01-0x06, 0x0F, 0x10）
- 被动监听模式：串口总线抓包、时序分帧、请求/响应自动配对
- 通信记录表格，支持报文详情查看
- 寄存器地址映射：自定义寄存器名称、单位、缩放系数
- 字节序切换：AB / BA / ABCD / CDAB / BADC / DCBA
- 导出 CSV / JSON
- 响应超时标记
- 结构化错误码，每条错误附带排查建议

## 截图

（待补充）

## 环境要求

- Node.js >= 18
- Rust >= 1.70
- 系统依赖见 [Tauri 官方文档](https://v2.tauri.app/start/prerequisites/)

## 开发

```bash
# 安装前端依赖
npm install

# 启动开发模式（前端 + Rust 后端热重载）
npm run tauri dev
```

## 构建

```bash
npm run tauri build
```

产物在 `src-tauri/target/release/bundle/` 下，包含各平台安装包。

## 项目结构

```
src/                  # React 前端
  App.tsx             # 主界面
  App.css             # 样式
src-tauri/            # Rust 后端
  src/lib.rs          # Modbus 协议逻辑、连接管理、监听引擎
  tauri.conf.json     # Tauri 配置
```

## 错误码速查

| 范围 | 类别 |
|------|------|
| E1xxx | 输入校验（从站地址、功能码、数量、地址溢出） |
| E2xxx | RTU 串口（发送、读取、超时、连接配置） |
| E3xxx | TCP（发送、读取、超时、MBAP 校验） |
| E4xxx | 连接状态 |
| E5xxx | 系统级（串口枚举） |
| E6xxx | 导出 |
| E7xxx | 监听模式 |
| E8xxx | 寄存器映射存储 |
| E9xxx | 报文分析 |

## 许可证

MIT
