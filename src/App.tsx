import { useState, useEffect, useRef, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
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

const BAUD_RATES = [9600, 19200, 38400, 57600, 115200];
const DATA_BITS = [7, 8];
const PARITIES = ["None", "Even", "Odd"];
const STOP_BITS = [1, 2];

function formatHex(bytes: number[]): string {
  return bytes.map((b) => b.toString(16).toUpperCase().padStart(2, "0")).join(" ");
}

function formatTimestamp(ts: number): string {
  const d = new Date(ts);
  return d.toLocaleTimeString("zh-CN", { hour12: false }) + "." + String(d.getMilliseconds()).padStart(3, "0");
}

function App() {
  const [ports, setPorts] = useState<PortInfo[]>([]);
  const [selectedPort, setSelectedPort] = useState("");
  const [config, setConfig] = useState<SerialConfig>({
    baud_rate: 9600,
    data_bits: 8,
    parity: "None",
    stop_bits: 1,
  });
  const [connected, setConnected] = useState(false);
  const [packets, setPackets] = useState<{ ts: number; hex: string; raw: number[] }[]>([]);
  const [error, setError] = useState("");
  const listRef = useRef<HTMLDivElement>(null);
  const autoScroll = useRef(true);

  const refreshPorts = useCallback(async () => {
    try {
      const result = await invoke<PortInfo[]>("list_ports");
      setPorts(result);
      setError("");
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    refreshPorts();
  }, [refreshPorts]);

  useEffect(() => {
    const unlisten = listen<{ ts: number; data: number[] }>("serial-data", (event) => {
      setPackets((prev) => {
        const next = [
          ...prev,
          { ts: event.payload.ts, hex: formatHex(event.payload.data), raw: event.payload.data },
        ];
        if (next.length > 10000) return next.slice(next.length - 10000);
        return next;
      });
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  useEffect(() => {
    if (autoScroll.current && listRef.current) {
      listRef.current.scrollTop = listRef.current.scrollHeight;
    }
  }, [packets]);

  const handleConnect = async () => {
    try {
      await invoke("connect_port", { portName: selectedPort, config });
      setConnected(true);
      setError("");
    } catch (e) {
      setError(String(e));
    }
  };

  const handleDisconnect = async () => {
    try {
      await invoke("disconnect_port");
      setConnected(false);
      setError("");
    } catch (e) {
      setError(String(e));
    }
  };

  const handleClear = () => setPackets([]);

  const handleScroll = () => {
    if (!listRef.current) return;
    const { scrollTop, scrollHeight, clientHeight } = listRef.current;
    autoScroll.current = scrollHeight - scrollTop - clientHeight < 40;
  };

  return (
    <div className="app">
      <header className="toolbar">
        <h1>myModebus</h1>
        <div className="connection-panel">
          <select
            value={selectedPort}
            onChange={(e) => setSelectedPort(e.target.value)}
            disabled={connected}
          >
            <option value="">选择串口...</option>
            {ports.map((p) => (
              <option key={p.name} value={p.name}>
                {p.name} ({p.port_type})
              </option>
            ))}
          </select>
          <button onClick={refreshPorts} disabled={connected} title="刷新串口列表">
            ↻
          </button>

          <select
            value={config.baud_rate}
            onChange={(e) => setConfig({ ...config, baud_rate: Number(e.target.value) })}
            disabled={connected}
          >
            {BAUD_RATES.map((b) => (
              <option key={b} value={b}>{b}</option>
            ))}
          </select>

          <select
            value={config.data_bits}
            onChange={(e) => setConfig({ ...config, data_bits: Number(e.target.value) })}
            disabled={connected}
          >
            {DATA_BITS.map((d) => (
              <option key={d} value={d}>{d}位</option>
            ))}
          </select>

          <select
            value={config.parity}
            onChange={(e) => setConfig({ ...config, parity: e.target.value })}
            disabled={connected}
          >
            {PARITIES.map((p) => (
              <option key={p} value={p}>{p}</option>
            ))}
          </select>

          <select
            value={config.stop_bits}
            onChange={(e) => setConfig({ ...config, stop_bits: Number(e.target.value) })}
            disabled={connected}
          >
            {STOP_BITS.map((s) => (
              <option key={s} value={s}>{s}停止位</option>
            ))}
          </select>

          {connected ? (
            <button className="btn-disconnect" onClick={handleDisconnect}>断开</button>
          ) : (
            <button
              className="btn-connect"
              onClick={handleConnect}
              disabled={!selectedPort}
            >
              连接
            </button>
          )}
        </div>

        <div className="status-bar">
          <span className={connected ? "status-on" : "status-off"}>
            {connected ? "● 已连接" : "○ 未连接"}
          </span>
          <span>{packets.length} 条数据</span>
          <button onClick={handleClear}>清空</button>
        </div>
      </header>

      {error && <div className="error-bar">{error}</div>}

      <div className="packet-list" ref={listRef} onScroll={handleScroll}>
        <table>
          <thead>
            <tr>
              <th style={{ width: "100px" }}>时间</th>
              <th style={{ width: "60px" }}>长度</th>
              <th>数据 (HEX)</th>
            </tr>
          </thead>
          <tbody>
            {packets.map((p, i) => (
              <tr key={i}>
                <td className="mono">{formatTimestamp(p.ts)}</td>
                <td className="mono">{p.raw.length}</td>
                <td className="mono hex-data">{p.hex}</td>
              </tr>
            ))}
          </tbody>
        </table>
        {packets.length === 0 && (
          <div className="empty-state">
            {connected ? "等待数据..." : "选择串口并连接以开始捕获"}
          </div>
        )}
      </div>
    </div>
  );
}

export default App;
