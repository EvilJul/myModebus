import { useState, useEffect, useRef, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";
import "./App.css";

interface PortInfo {
  name: string;
  port_type: string;
}

interface SerialConfig {
  baud_rate: number;
  data_bits: number;
  parity: string;
  stop_bits: number;
}

interface ModbusResponse {
  ts: number;
  slave_id: number;
  function_code: number;
  is_exception: boolean;
  exception_code: number | null;
  exception_text: string | null;
  data: number[];
  raw_request: number[];
  raw_response: number[];
  crc_ok: boolean;
  elapsed_ms: number;
  mode: string;
  slow: boolean;
}

interface MonitorFrame {
  ts: number;
  raw: number[];
  slave_id: number;
  function_code: number;
  is_exception: boolean;
  exception_code: number | null;
  exception_text: string | null;
  data: number[];
  crc_ok: boolean;
}

interface MonitorPair {
  request: MonitorFrame | null;
  response: MonitorFrame | null;
  elapsed_ms: number;
  retry_hint: boolean;
}

type ConnMode = "RTU" | "TCP";
type WorkMode = "query" | "monitor";

interface RegisterMap {
  address: number;
  name: string;
  unit: string;
  scale: number;
}

type ByteOrder = "AB" | "BA" | "ABCD" | "CDAB" | "BADC" | "DCBA";

const BYTE_ORDERS: { value: ByteOrder; label: string }[] = [
  { value: "AB", label: "AB (Big Endian 16位)" },
  { value: "BA", label: "BA (Little Endian 16位)" },
  { value: "ABCD", label: "ABCD (Big Endian 32位)" },
  { value: "CDAB", label: "CDAB (Word Swap 32位)" },
  { value: "BADC", label: "BADC (Byte Swap 32位)" },
  { value: "DCBA", label: "DCBA (Little Endian 32位)" },
];

function applyByteOrder(data: number[], order: ByteOrder): number[] {
  if (order === "AB") return data;
  if (order === "BA") return data.map((v) => ((v & 0xFF) << 8) | ((v >> 8) & 0xFF));
  // 32位模式: 每两个寄存器组合
  const result: number[] = [];
  for (let i = 0; i < data.length; i += 2) {
    if (i + 1 >= data.length) { result.push(data[i]); break; }
    const hi = data[i], lo = data[i + 1];
    let val32: number;
    switch (order) {
      case "ABCD": val32 = (hi << 16) | lo; break;
      case "CDAB": val32 = (lo << 16) | hi; break;
      case "BADC": val32 = (((hi & 0xFF) << 8 | (hi >> 8)) << 16) | ((lo & 0xFF) << 8 | (lo >> 8)); break;
      case "DCBA": val32 = (((lo & 0xFF) << 8 | (lo >> 8)) << 16) | ((hi & 0xFF) << 8 | (hi >> 8)); break;
      default: val32 = (hi << 16) | lo;
    }
    result.push(val32 >>> 0); // unsigned
  }
  return result;
}

const BAUD_RATES = [9600, 19200, 38400, 57600, 115200];
const DATA_BITS = [7, 8];
const PARITIES = ["None", "Even", "Odd"];
const STOP_BITS = [1, 2];

const FUNCTION_CODES = [
  { code: 0x01, name: "01 读线圈", type: "read" },
  { code: 0x02, name: "02 读离散输入", type: "read" },
  { code: 0x03, name: "03 读保持寄存器", type: "read" },
  { code: 0x04, name: "04 读输入寄存器", type: "read" },
  { code: 0x05, name: "05 写单个线圈", type: "write-single" },
  { code: 0x06, name: "06 写单个寄存器", type: "write-single" },
  { code: 0x0f, name: "0F 写多个线圈", type: "write-multiple" },
  { code: 0x10, name: "10 写多个寄存器", type: "write-multiple" },
];

function formatHex(bytes: number[]): string {
  return bytes.map((b) => b.toString(16).toUpperCase().padStart(2, "0")).join(" ");
}

function formatTimestamp(ts: number): string {
  const d = new Date(ts);
  return d.toLocaleTimeString("zh-CN", { hour12: false }) + "." + String(d.getMilliseconds()).padStart(3, "0");
}

function fcName(fc: number): string {
  const entry = FUNCTION_CODES.find((f) => f.code === (fc & 0x7f));
  if (fc & 0x80) return `异常 (0x${fc.toString(16).toUpperCase()})`;
  return entry ? entry.name : `0x${fc.toString(16).toUpperCase()}`;
}

function App() {
  const [mode, setMode] = useState<ConnMode>("RTU");
  const [ports, setPorts] = useState<PortInfo[]>([]);
  const [selectedPort, setSelectedPort] = useState("");
  const [serialConfig, setSerialConfig] = useState<SerialConfig>({
    baud_rate: 9600, data_bits: 8, parity: "None", stop_bits: 1,
  });
  const [tcpIp, setTcpIp] = useState("192.168.1.1");
  const [tcpPort, setTcpPort] = useState(502);
  const [connected, setConnected] = useState(false);
  const [error, setError] = useState("");

  const [slaveId, setSlaveId] = useState(1);
  const [functionCode, setFunctionCode] = useState(0x03);
  const [startAddress, setStartAddress] = useState(0);
  const [quantity, setQuantity] = useState(10);
  const [writeValue, setWriteValue] = useState("0");
  const [sending, setSending] = useState(false);
  const [workMode, setWorkMode] = useState<WorkMode>("query");
  const [monitoring, setMonitoring] = useState(false);
  const [monitorPairs, setMonitorPairs] = useState<MonitorPair[]>([]);
  const [regMaps, setRegMaps] = useState<RegisterMap[]>([]);
  const [showRegEditor, setShowRegEditor] = useState(false);
  const [byteOrder, setByteOrder] = useState<ByteOrder>("AB");
  const [aiConfig, setAiConfig] = useState({ api_url: "", api_key: "", model: "" });
  const [aiResult, setAiResult] = useState("");
  const [aiLoading, setAiLoading] = useState(false);
  const [showAiPanel, setShowAiPanel] = useState(false);

  const [transactions, setTransactions] = useState<ModbusResponse[]>([]);
  const [selectedTx, setSelectedTx] = useState<ModbusResponse | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const autoScroll = useRef(true);

  const fcType = FUNCTION_CODES.find((f) => f.code === functionCode)?.type ?? "read";

  const refreshPorts = useCallback(async () => {
    try {
      const result = await invoke<PortInfo[]>("list_ports");
      setPorts(result);
      setError("");
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => { refreshPorts(); }, [refreshPorts]);

  // 加载寄存器映射
  useEffect(() => {
    invoke<RegisterMap[]>("load_register_map").then(setRegMaps).catch(() => {});
    invoke<{api_url:string;api_key:string;model:string}>("load_ai_config").then(setAiConfig).catch(() => {});
  }, []);

  // 监听模式事件
  useEffect(() => {
    let unlisten: UnlistenFn | null = null;
    if (monitoring) {
      listen<MonitorPair>("monitor-frame", (event) => {
        setMonitorPairs((prev) => {
          const next = [...prev, event.payload];
          return next.length > 10000 ? next.slice(next.length - 10000) : next;
        });
      }).then((fn) => { unlisten = fn; });
    }
    return () => { if (unlisten) unlisten(); };
  }, [monitoring]);

  useEffect(() => {
    if (autoScroll.current && listRef.current) {
      listRef.current.scrollTop = listRef.current.scrollHeight;
    }
  }, [transactions]);

  const handleConnect = async () => {
    try {
      if (mode === "RTU") {
        await invoke("connect_port", { portName: selectedPort, config: serialConfig });
      } else {
        await invoke("connect_tcp", { config: { ip: tcpIp, port: tcpPort } });
      }
      setConnected(true);
      setError("");
    } catch (e) {
      setError(String(e));
    }
  };

  const handleDisconnect = async () => {
    try {
      await invoke("disconnect");
      setConnected(false);
      setError("");
    } catch (e) {
      setError(String(e));
    }
  };

  const handleSend = async () => {
    if (!connected || sending) return;

    // 前端输入校验
    if (slaveId < 1 || slaveId > 247) {
      setError("从站地址无效，有效范围 1-247");
      return;
    }
    if (startAddress < 0 || startAddress > 65535) {
      setError("起始地址无效，有效范围 0-65535");
      return;
    }
    if (fcType === "read" && (quantity < 1 || quantity > 2000)) {
      setError("读取数量无效，有效范围 1-2000");
      return;
    }
    if (fcType === "write-single") {
      const v = parseInt(writeValue, 10);
      if (isNaN(v)) { setError("写入值必须是数字"); return; }
      if (functionCode === 0x06 && (v < 0 || v > 65535)) {
        setError("寄存器值无效，有效范围 0-65535");
        return;
      }
    }
    if (fcType === "write-multiple") {
      const vals = writeValue.split(/[,\s]+/).filter(Boolean);
      if (vals.length === 0) { setError("写多个值时至少提供一个值"); return; }
      for (const s of vals) {
        const v = parseInt(s, 10);
        if (isNaN(v)) { setError(`写入值 "${s}" 不是有效数字`); return; }
      }
    }

    setSending(true);
    setError("");
    try {
      let writeValues: number[] | undefined;
      if (fcType === "write-single") {
        writeValues = [parseInt(writeValue, 10) || 0];
      } else if (fcType === "write-multiple") {
        writeValues = writeValue.split(/[,\s]+/).filter(Boolean).map((v) => parseInt(v, 10) || 0);
      }
      const resp = await invoke<ModbusResponse>("modbus_query", {
        request: {
          slave_id: slaveId,
          function_code: functionCode,
          start_address: startAddress,
          quantity: fcType === "write-multiple" ? (writeValues?.length ?? quantity) : quantity,
          write_values: writeValues,
        },
      });
      setTransactions((prev) => {
        const next = [...prev, resp];
        return next.length > 10000 ? next.slice(next.length - 10000) : next;
      });
    } catch (e) {
      const msg = String(e);
      setError(msg);
      // 连接类错误自动断开状态
      if (msg.includes("[E2001]") || msg.includes("[E3001]") || msg.includes("[E3004]") || msg.includes("连接可能已断开")) {
        setConnected(false);
      }
    } finally {
      setSending(false);
    }
  };

  const handleClear = () => { setTransactions([]); setMonitorPairs([]); setSelectedTx(null); };

  const handleStartMonitor = async () => {
    if (!connected || mode !== "RTU") { setError("被动监听仅支持 RTU 串口模式"); return; }
    try {
      await invoke("start_monitor", { baudRate: serialConfig.baud_rate });
      setMonitoring(true);
      setError("");
    } catch (e) { setError(String(e)); }
  };

  const handleStopMonitor = async () => {
    try {
      await invoke("stop_monitor");
      setMonitoring(false);
      setConnected(false); // 监听结束后串口已释放，需重新连接
      setError("");
    } catch (e) { setError(String(e)); }
  };

  const getRegName = (addr: number) => regMaps.find((r) => r.address === addr);

  const handleAddReg = () => {
    const addr = parseInt(prompt("寄存器地址 (0-65535):") || "", 10);
    if (isNaN(addr) || addr < 0 || addr > 65535) return;
    const name = prompt("名称 (如: 温度):") || "";
    if (!name) return;
    const unit = prompt("单位 (如: °C，可留空):") || "";
    const scaleStr = prompt("缩放系数 (如: 0.1，默认1):") || "1";
    const scale = parseFloat(scaleStr) || 1;
    const updated = [...regMaps.filter((r) => r.address !== addr), { address: addr, name, unit, scale }];
    updated.sort((a, b) => a.address - b.address);
    setRegMaps(updated);
    invoke("save_register_map", { maps: updated }).catch((e) => setError(String(e)));
  };

  const handleRemoveReg = (addr: number) => {
    const updated = regMaps.filter((r) => r.address !== addr);
    setRegMaps(updated);
    invoke("save_register_map", { maps: updated }).catch((e) => setError(String(e)));
  };

  const handleAiAnalyze = async () => {
    if (!aiConfig.api_url || !aiConfig.api_key) { setError("请先配置 AI API 地址和密钥"); return; }
    const data = workMode === "monitor" ? monitorPairs : transactions;
    if (data.length === 0) { setError("没有可分析的通信记录"); return; }
    const last50 = data.slice(-50);
    const context = JSON.stringify(last50, null, 1);
    setAiLoading(true);
    setAiResult("");
    try {
      const result = await invoke<string>("ai_analyze", { config: aiConfig, context });
      setAiResult(result);
    } catch (e) { setError(String(e)); }
    finally { setAiLoading(false); }
  };

  const handleSaveAiConfig = () => {
    invoke("save_ai_config", { config: aiConfig }).then(() => setError("")).catch((e) => setError(String(e)));
  };

  const handleExport = async (format: "csv" | "json") => {
    const isMonitor = workMode === "monitor";
    const hasData = isMonitor ? monitorPairs.length > 0 : transactions.length > 0;
    if (!hasData) { setError("没有可导出的记录"); return; }
    let content: string;
    if (format === "csv") {
      if (isMonitor) {
        const header = "序号,时间,从站,功能码,配对,异常,数据,请求HEX,响应HEX,校验,耗时ms";
        const rows = monitorPairs.map((pair, i) => {
          const frame = pair.response ?? pair.request;
          if (!frame) return "";
          const time = formatTimestamp(frame.ts);
          const fc = `0x${frame.function_code.toString(16).toUpperCase().padStart(2, "0")}`;
          const paired = pair.request && pair.response ? "是" : "否";
          const exc = frame.is_exception ? `${frame.exception_code} ${frame.exception_text}` : "";
          const data = frame.data.join(";");
          const reqHex = pair.request ? formatHex(pair.request.raw) : "";
          const resHex = pair.response ? formatHex(pair.response.raw) : "";
          const crcOk = (pair.request?.crc_ok ?? true) && (pair.response?.crc_ok ?? true);
          return `${i + 1},"${time}",${frame.slave_id},${fc},${paired},"${exc}","${data}","${reqHex}","${resHex}",${crcOk ? "OK" : "FAIL"},${pair.elapsed_ms}`;
        });
        content = "\uFEFF" + header + "\n" + rows.join("\n");
      } else {
        const header = "序号,时间,模式,从站,功能码,异常,数据,请求HEX,响应HEX,校验,耗时ms";
        const rows = transactions.map((tx, i) => {
          const time = formatTimestamp(tx.ts);
          const fc = `0x${tx.function_code.toString(16).toUpperCase().padStart(2, "0")}`;
          const exc = tx.is_exception ? `${tx.exception_code} ${tx.exception_text}` : "";
          const data = tx.data.join(";");
          const reqHex = formatHex(tx.raw_request);const resHex = formatHex(tx.raw_response);
          return `${i + 1},"${time}",${tx.mode},${tx.slave_id},${fc},"${exc}","${data}","${reqHex}","${resHex}",${tx.crc_ok ? "OK" : "FAIL"},${tx.elapsed_ms}`;
        });
        content = "\uFEFF" + header + "\n" + rows.join("\n");
      }
    } else {
      content = JSON.stringify(isMonitor ? monitorPairs : transactions, null, 2);
    }
    try {
      const path = await invoke<string>("export_log", { content, format });
      setError("");
      alert(`已导出到: ${path}`);
    } catch (e) {
      const msg = String(e);
      if (!msg.includes("[E6003]")) setError(msg);
    }
  };

  const handleScroll = () => {
    if (!listRef.current) return;
    const { scrollTop, scrollHeight, clientHeight } = listRef.current;
    autoScroll.current = scrollHeight - scrollTop - clientHeight < 40;
  };

  const canConnect = mode === "RTU" ? !!selectedPort : (!!tcpIp && tcpPort > 0);

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="sidebar-header">
          <h1>myModebus</h1>
          <span className="version">v0.1.0</span>
        </div>

        {/* 连接模式 */}
        <div className="sidebar-section">
          <div className="section-title">连接</div>
          <div className="mode-tabs">
            <button className={`mode-tab ${mode === "RTU" ? "active" : ""}`} onClick={() => { if (!connected) setMode("RTU"); }} disabled={connected}>RTU</button>
            <button className={`mode-tab ${mode === "TCP" ? "active" : ""}`} onClick={() => { if (!connected) setMode("TCP"); }} disabled={connected}>TCP</button>
          </div>

          {mode === "RTU" ? (
            <>
              <div className="field">
                <label>串口</label>
                <div className="port-row">
                  <select value={selectedPort} onChange={(e) => setSelectedPort(e.target.value)} disabled={connected}>
                    <option value="">选择串口...</option>
                    {ports.map((p) => (
                      <option key={p.name} value={p.name}>{p.name} ({p.port_type})</option>
                    ))}
                  </select>
                  <button className="btn-icon" onClick={refreshPorts} disabled={connected} title="刷新">
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                      <path d="M21.5 2v6h-6M2.5 22v-6h6M2 11.5a10 10 0 0 1 18.8-4.3M22 12.5a10 10 0 0 1-18.8 4.2"/>
                    </svg>
                  </button>
                </div>
              </div>
              <div className="field">
                <label>波特率</label>
                <select value={serialConfig.baud_rate} onChange={(e) => setSerialConfig({ ...serialConfig, baud_rate: Number(e.target.value) })} disabled={connected}>
                  {BAUD_RATES.map((b) => (<option key={b} value={b}>{b}</option>))}
                </select>
              </div>
              <div className="field-row">
                <div className="field">
                  <label>数据位</label>
                  <select value={serialConfig.data_bits} onChange={(e) => setSerialConfig({ ...serialConfig, data_bits: Number(e.target.value) })} disabled={connected}>
                    {DATA_BITS.map((d) => (<option key={d} value={d}>{d}</option>))}
                  </select>
                </div>
                <div className="field">
                  <label>停止位</label>
                  <select value={serialConfig.stop_bits} onChange={(e) => setSerialConfig({ ...serialConfig, stop_bits: Number(e.target.value) })} disabled={connected}>
                    {STOP_BITS.map((s) => (<option key={s} value={s}>{s}</option>))}
                  </select>
                </div>
              </div>
              <div className="field">
                <label>校验位</label>
                <select value={serialConfig.parity} onChange={(e) => setSerialConfig({ ...serialConfig, parity: e.target.value })} disabled={connected}>
                  {PARITIES.map((p) => (<option key={p} value={p}>{p === "None" ? "无" : p === "Even" ? "偶校验" : "奇校验"}</option>))}
                </select>
              </div>
            </>
          ) : (
            <>
              <div className="field">
                <label>IP 地址</label>
                <input type="text" value={tcpIp} onChange={(e) => setTcpIp(e.target.value)} disabled={connected} placeholder="192.168.1.1" />
              </div>
              <div className="field">
                <label>端口</label>
                <input type="number" min={1} max={65535} value={tcpPort} onChange={(e) => setTcpPort(Number(e.target.value))} disabled={connected} />
              </div>
            </>
          )}

          {connected ? (
            <button className="btn-action btn-disconnect" onClick={handleDisconnect}>断开连接</button>
          ) : (
            <button className="btn-action btn-connect" onClick={handleConnect} disabled={!canConnect}>连接</button>
          )}
        </div>

        {/* 工作模式 */}
        {connected && mode === "RTU" && (
          <div className="sidebar-section">
            <div className="section-title">工作模式</div>
            <div className="mode-tabs">
              <button className={`mode-tab ${workMode === "query" ? "active" : ""}`} onClick={() => { if (!monitoring) setWorkMode("query"); }} disabled={monitoring}>查询</button>
              <button className={`mode-tab ${workMode === "monitor" ? "active" : ""}`} onClick={() => { if (!monitoring) setWorkMode("monitor"); }} disabled={monitoring}>监听</button>
            </div>
            {workMode === "monitor" && (
              monitoring ? (
                <button className="btn-action btn-disconnect" onClick={handleStopMonitor}>停止监听</button>
              ) : (
                <button className="btn-action btn-connect" onClick={handleStartMonitor}>开始监听</button>
              )
            )}
          </div>
        )}

        {/* Modbus 查询 - 仅查询模式显示 */}
        {workMode === "query" && (
        <div className="sidebar-section">
          <div className="section-title">Modbus 查询</div>
          <div className="field">
            <label>从站地址</label>
            <input type="number" min={1} max={247} value={slaveId} onChange={(e) => setSlaveId(Number(e.target.value))} disabled={!connected} />
          </div>
          <div className="field">
            <label>功能码</label>
            <select value={functionCode} onChange={(e) => setFunctionCode(Number(e.target.value))} disabled={!connected}>
              {FUNCTION_CODES.map((f) => (<option key={f.code} value={f.code}>{f.name}</option>))}
            </select>
          </div>
          <div className="field">
            <label>起始地址</label>
            <input type="number" min={0} max={65535} value={startAddress} onChange={(e) => setStartAddress(Number(e.target.value))} disabled={!connected} />
          </div>
          {fcType === "read" && (
            <div className="field">
              <label>数量</label>
              <input type="number" min={1} max={125} value={quantity} onChange={(e) => setQuantity(Number(e.target.value))} disabled={!connected} />
            </div>
          )}
          {fcType === "write-single" && (
            <div className="field">
              <label>{functionCode === 0x05 ? "值 (0=OFF, 1=ON)" : "值"}</label>
              <input type="text" value={writeValue} onChange={(e) => setWriteValue(e.target.value)} disabled={!connected} />
            </div>
          )}
          {fcType === "write-multiple" && (
            <div className="field">
              <label>值 (逗号分隔)</label>
              <input type="text" value={writeValue} onChange={(e) => setWriteValue(e.target.value)} disabled={!connected} placeholder="100, 200, 300" />
            </div>
          )}
          <button className="btn-action btn-send" onClick={handleSend} disabled={!connected || sending}>
            {sending ? "发送中..." : "发送"}
          </button>
        </div>
        )}

        {/* 状态 */}
        <div className="sidebar-section">
          <div className="section-title">状态</div>
          <div className="status-info">
            <div className="status-row">
              <span className="status-label">连接</span>
              <span className={connected ? "status-dot connected" : "status-dot"}>
                {connected ? `已连接 (${mode})` : "未连接"}
              </span>
            </div>
            <div className="status-row">
              <span className="status-label">事务数</span>
              <span className="status-value mono">{workMode === "monitor" ? monitorPairs.length : transactions.length}</span>
            </div>
            {monitoring && (
              <div className="status-row">
                <span className="status-label">监听</span>
                <span className="status-dot connected">运行中</span>
              </div>
            )}
          </div>
        </div>

        {error && (
          <div className="sidebar-section">
            <div className="error-box">{error}</div>
          </div>
        )}

        {/* 寄存器映射 */}
        <div className="sidebar-section">
          <div className="section-title" style={{display:"flex",justifyContent:"space-between",alignItems:"center"}}>
            <span>寄存器映射 ({regMaps.length})</span>
            <button className="btn-icon-sm" onClick={() => setShowRegEditor(!showRegEditor)} title="展开/收起">{showRegEditor ? "−" : "+"}</button>
          </div>
          {showRegEditor && (
            <>
              <div className="reg-map-list">
                {regMaps.map((r) => (
                  <div key={r.address} className="reg-map-item">
                    <span className="mono">{r.address}</span>
                    <span>{r.name}{r.unit ? ` (${r.unit})` : ""}{r.scale !== 1 ? ` ×${r.scale}` : ""}</span>
                    <button className="btn-icon-sm" onClick={() => handleRemoveReg(r.address)} title="删除">×</button>
                  </div>
                ))}
              </div>
              <button className="btn-action btn-connect" onClick={handleAddReg} style={{marginTop:4}}>添加映射</button>
            </>
          )}
        </div>

        {/* AI 分析 */}
        <div className="sidebar-section">
          <div className="section-title" style={{display:"flex",justifyContent:"space-between",alignItems:"center"}}>
            <span>AI 分析</span>
            <button className="btn-icon-sm" onClick={() => setShowAiPanel(!showAiPanel)} title="展开/收起">{showAiPanel ? "−" : "+"}</button>
          </div>
          {showAiPanel && (
            <>
              <div className="field">
                <label>API 地址</label>
                <input type="text" value={aiConfig.api_url} onChange={(e) => setAiConfig({...aiConfig, api_url: e.target.value})} placeholder="https://api.openai.com/v1/chat/completions" />
              </div>
              <div className="field">
                <label>API Key</label>
                <input type="password" value={aiConfig.api_key} onChange={(e) => setAiConfig({...aiConfig, api_key: e.target.value})} placeholder="sk-..." />
              </div>
              <div className="field">
                <label>模型</label>
                <input type="text" value={aiConfig.model} onChange={(e) => setAiConfig({...aiConfig, model: e.target.value})} placeholder="gpt-4o" />
              </div>
              <div style={{display:"flex",gap:6}}>
                <button className="btn-action btn-connect" onClick={handleSaveAiConfig} style={{flex:1}}>保存配置</button>
                <button className="btn-action btn-send" onClick={handleAiAnalyze} disabled={aiLoading} style={{flex:1}}>
                  {aiLoading ? "分析中..." : "分析报文"}
                </button>
              </div>
            </>
          )}
        </div>
      </aside>

      <main className="main">
        <div className="main-toolbar">
          <span className="main-title">{workMode === "monitor" ? "监听记录" : "通信记录"}{monitoring && " (实时)"}</span>
          <div className="main-actions">
            <button className="btn-text" onClick={() => handleExport("csv")} disabled={(workMode === "monitor" ? monitorPairs : transactions).length === 0}>导出 CSV</button>
            <button className="btn-text" onClick={() => handleExport("json")} disabled={(workMode === "monitor" ? monitorPairs : transactions).length === 0}>导出 JSON</button>
            <button className="btn-text" onClick={handleClear}>清空</button>
            {aiResult && <button className="btn-text" onClick={() => setAiResult("")}>关闭AI结果</button>}
          </div>
        </div>

        <div className="main-content">
          <div className="packet-list" ref={listRef} onScroll={handleScroll}>
            <table>
              <thead>
                <tr>
                  <th className="col-index">#</th>
                  <th className="col-time">时间</th>
                  <th className="col-mode">模式</th>
                  <th className="col-slave">从站</th>
                  <th className="col-fc">功能码</th>
                  <th className="col-data">数据</th>
                  <th className="col-crc">校验</th>
                  <th className="col-ms">耗时</th>
                </tr>
              </thead>
              <tbody>
                {workMode === "query" ? (
                  transactions.map((tx, i) => (
                    <tr
                      key={i}
                      className={`${tx.is_exception ? "row-error" : ""} ${!tx.crc_ok ? "row-crc-error" : ""} ${tx.slow ? "row-slow" : ""} ${selectedTx === tx ? "row-selected" : ""}`}
                      onClick={() => setSelectedTx(tx)}
                    >
                      <td className="mono col-index">{i + 1}</td>
                      <td className="mono col-time">{formatTimestamp(tx.ts)}</td>
                      <td className="col-mode">
                        <span className={`mode-badge mode-${tx.mode.toLowerCase()}`}>{tx.mode}</span>
                      </td>
                      <td className="mono col-slave">{tx.slave_id}</td>
                      <td className="col-fc">
                        <span className={`fc-badge ${tx.is_exception ? "fc-exception" : ""}`}>{fcName(tx.function_code)}</span>
                      </td>
                      <td className="mono col-data">
                        {tx.is_exception ? tx.exception_text : tx.data.map((d) => d.toString()).join(", ")}
                      </td>
                      <td className="col-crc">
                        <span className={tx.crc_ok ? "crc-ok" : "crc-fail"}>
                          {tx.crc_ok ? "OK" : "ERR"}
                        </span>
                      </td>
                      <td className="mono col-ms">{tx.elapsed_ms}ms{tx.slow && " ⚠"}</td>
                    </tr>
                  ))
                ) : (
                  monitorPairs.map((pair, i) => {
                    const frame = pair.response ?? pair.request;
                    if (!frame) return null;
                    const hasReq = !!pair.request;
                    const hasRes = !!pair.response;
                    const isExc = frame.is_exception;
                    const crcOk = (pair.request?.crc_ok ?? true) && (pair.response?.crc_ok ?? true);
                    return (
                      <tr key={i} className={`${isExc ? "row-error" : ""} ${!crcOk ? "row-crc-error" : ""}`}>
                        <td className="mono col-index">{i + 1}</td>
                        <td className="mono col-time">{formatTimestamp(frame.ts)}</td>
                        <td className="col-mode">
                          <span className={`mode-badge ${pair.retry_hint ? "mode-tcp" : "mode-rtu"}`}>{hasReq && hasRes ? (pair.retry_hint ? "重试" : "配对") : "孤立"}</span>
                        </td>
                        <td className="mono col-slave">{frame.slave_id}</td>
                        <td className="col-fc">
                          <span className={`fc-badge ${isExc ? "fc-exception" : ""}`}>{fcName(frame.function_code)}</span>
                        </td>
                        <td className="mono col-data">
                          {isExc ? frame.exception_text : frame.data.map((d) => d.toString()).join(", ")}
                        </td>
                        <td className="col-crc">
                          <span className={crcOk ? "crc-ok" : "crc-fail"}>{crcOk ? "OK" : "ERR"}</span>
                        </td>
                        <td className="mono col-ms">{pair.elapsed_ms}ms</td>
                      </tr>
                    );
                  })
                )}
              </tbody>
            </table>
            {(workMode === "query" ? transactions.length : monitorPairs.length) === 0 && (
              <div className="empty-state">
                <p>{workMode === "monitor"
                  ? (monitoring ? "等待总线数据..." : "点击「开始监听」捕获总线报文")
                  : (connected ? "配置参数后点击「发送」开始查询" : "连接串口或 TCP 后开始 Modbus 通信")
                }</p>
              </div>
            )}
          </div>

          {selectedTx && (
            <div className="detail-panel">
              <div className="detail-header">
                <span>报文详情 ({selectedTx.mode})</span>
                <button className="btn-icon-sm" onClick={() => setSelectedTx(null)} title="关闭">×</button>
              </div>
              <div className="detail-body">
                <div className="detail-section">
                  <div className="detail-label">请求 (TX)</div>
                  <div className="detail-hex mono">{formatHex(selectedTx.raw_request)}</div>
                </div>
                <div className="detail-section">
                  <div className="detail-label">响应 (RX)</div>
                  <div className={`detail-hex mono ${!selectedTx.crc_ok ? "hex-error" : ""}`}>
                    {formatHex(selectedTx.raw_response)}
                  </div>
                </div>
                <div className="detail-section">
                  <div className="detail-label">解析</div>
                  <div className="detail-grid">
                    <span className="detail-key">通信模式</span>
                    <span className="detail-val">{selectedTx.mode === "RTU" ? "Modbus RTU (串口)" : "Modbus TCP"}</span>
                    <span className="detail-key">从站地址</span>
                    <span className="detail-val mono">{selectedTx.slave_id}</span>
                    <span className="detail-key">功能码</span>
                    <span className="detail-val">{fcName(selectedTx.function_code)}</span>
                    <span className="detail-key">{selectedTx.mode === "RTU" ? "CRC 校验" : "MBAP 校验"}</span>
                    <span className={`detail-val ${selectedTx.crc_ok ? "text-ok" : "text-err"}`}>
                      {selectedTx.crc_ok ? "通过" : "失败"}
                    </span>
                    <span className="detail-key">响应耗时</span>
                    <span className="detail-val mono">{selectedTx.elapsed_ms} ms</span>
                    {selectedTx.is_exception && (
                      <>
                        <span className="detail-key">异常码</span>
                        <span className="detail-val text-err">
                          0x{selectedTx.exception_code?.toString(16).toUpperCase().padStart(2, "0")} {selectedTx.exception_text}
                        </span>
                      </>
                    )}
                  </div>
                </div>
                {!selectedTx.is_exception && selectedTx.data.length > 0 && (
                  <div className="detail-section">
                    <div className="detail-label" style={{display:"flex",justifyContent:"space-between",alignItems:"center"}}>
                      <span>寄存器值</span>
                      <select className="byte-order-select" value={byteOrder} onChange={(e) => setByteOrder(e.target.value as ByteOrder)}>
                        {BYTE_ORDERS.map((o) => <option key={o.value} value={o.value}>{o.label}</option>)}
                      </select>
                    </div>
                    <div className="register-table">
                      <table>
                        <thead>
                          <tr><th>地址</th><th>名称</th><th>十进制</th><th>十六进制</th></tr>
                        </thead>
                        <tbody>
                          {applyByteOrder(selectedTx.data, byteOrder).map((val, idx) => {
                            const is32 = byteOrder.length === 4;
                            const addr = startAddress + (is32 ? idx * 2 : idx);
                            const reg = getRegName(addr);
                            const scaled = reg && reg.scale !== 1 ? (val * reg.scale).toFixed(2) : val;
                            const hexWidth = is32 ? 8 : 4;
                            return (
                              <tr key={idx}>
                                <td className="mono">{addr}</td>
                                <td>{reg ? `${reg.name}${reg.unit ? ` (${reg.unit})` : ""}` : ""}</td>
                                <td className="mono">{scaled}</td>
                                <td className="mono">0x{(val >>> 0).toString(16).toUpperCase().padStart(hexWidth, "0")}</td>
                              </tr>
                            );
                          })}
                        </tbody>
                      </table>
                    </div>
                  </div>
                )}
              </div>
            </div>
          )}

          {aiResult && (
            <div className="detail-panel">
              <div className="detail-header">
                <span>AI 分析结果</span>
                <button className="btn-icon-sm" onClick={() => setAiResult("")} title="关闭">×</button>
              </div>
              <div className="detail-body">
                <div className="ai-result">{aiResult}</div>
              </div>
            </div>
          )}
        </div>
      </main>
    </div>
  );
}

export default App;
