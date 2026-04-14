use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};

// ---- CRC-16/Modbus ----

fn crc16_modbus(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if crc & 0x0001 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

fn append_crc(frame: &mut Vec<u8>) {
    let crc = crc16_modbus(frame);
    frame.push((crc & 0xFF) as u8);
    frame.push((crc >> 8) as u8);
}

fn verify_crc(frame: &[u8]) -> bool {
    if frame.len() < 4 {
        return false;
    }
    let crc = crc16_modbus(&frame[..frame.len() - 2]);
    let expected = (frame[frame.len() - 2] as u16) | ((frame[frame.len() - 1] as u16) << 8);
    crc == expected
}

// ---- Types ----

#[derive(Serialize, Clone)]
struct PortInfo {
    name: String,
    port_type: String,
}

#[derive(Deserialize, Clone)]
struct SerialConfig {
    baud_rate: u32,
    data_bits: u8,
    parity: String,
    stop_bits: u8,
}

#[derive(Deserialize)]
struct TcpConfig {
    ip: String,
    port: u16,
}

#[derive(Deserialize)]
struct ModbusRequest {
    slave_id: u8,
    function_code: u8,
    start_address: u16,
    quantity: u16,
    write_values: Option<Vec<u16>>,
}

#[derive(Serialize, Clone)]
struct ModbusResponse {
    ts: u64,
    slave_id: u8,
    function_code: u8,
    is_exception: bool,
    exception_code: Option<u8>,
    exception_text: Option<String>,
    data: Vec<u16>,
    raw_request: Vec<u8>,
    raw_response: Vec<u8>,
    crc_ok: bool,
    elapsed_ms: u64,
    mode: String,
    slow: bool, // 响应耗时超过阈值
}

// ---- Monitor types ----

#[derive(Serialize, Clone, Debug)]
struct MonitorFrame {
    ts: u64,
    raw: Vec<u8>,
    slave_id: u8,
    function_code: u8,
    is_exception: bool,
    exception_code: Option<u8>,
    exception_text: Option<String>,
    data: Vec<u16>,
    crc_ok: bool,
}

#[derive(Serialize, Clone)]
struct MonitorPair {
    request: Option<MonitorFrame>,
    response: Option<MonitorFrame>,
    elapsed_ms: u64,
    retry_hint: bool, // 可能是超时重试的迟到响应
}

struct PairState {
    pending: Option<MonitorFrame>,
    timeout_history: Vec<MonitorFrame>, // 最近 N 个已超时的请求
}

const PAIR_TIMEOUT_MS: u64 = 5000;
const MAX_TIMEOUT_HISTORY: usize = 16;

// ---- Connection abstraction ----

enum Connection {
    Serial(Box<dyn serialport::SerialPort>),
    Tcp(TcpStream),
}

struct AppState {
    connection: Mutex<Option<Connection>>,
    tcp_transaction_id: Mutex<u16>,
    monitor_running: Arc<AtomicBool>,
    monitor_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            connection: Mutex::new(None),
            tcp_transaction_id: Mutex::new(0),
            monitor_running: Arc::new(AtomicBool::new(false)),
            monitor_thread: Mutex::new(None),
        }
    }
}

// ---- Modbus PDU building (shared between RTU and TCP) ----

fn build_pdu(req: &ModbusRequest) -> Result<Vec<u8>, String> {
    match req.function_code {
        0x01 | 0x02 | 0x03 | 0x04 => {
            Ok(vec![
                req.function_code,
                (req.start_address >> 8) as u8,
                (req.start_address & 0xFF) as u8,
                (req.quantity >> 8) as u8,
                (req.quantity & 0xFF) as u8,
            ])
        }
        0x05 => {
            let val = req.write_values.as_ref()
                .and_then(|v| v.first()).copied().unwrap_or(0);
            let coil: u16 = if val != 0 { 0xFF00 } else { 0x0000 };
            Ok(vec![
                0x05,
                (req.start_address >> 8) as u8,
                (req.start_address & 0xFF) as u8,
                (coil >> 8) as u8,
                (coil & 0xFF) as u8,
            ])
        }
        0x06 => {
            let val = req.write_values.as_ref()
                .and_then(|v| v.first()).copied().unwrap_or(0);
            Ok(vec![
                0x06,
                (req.start_address >> 8) as u8,
                (req.start_address & 0xFF) as u8,
                (val >> 8) as u8,
                (val & 0xFF) as u8,
            ])
        }
        0x0F => {
            let values = req.write_values.as_deref().unwrap_or(&[]);
            let byte_count = ((req.quantity + 7) / 8) as u8;
            let mut pdu = vec![
                0x0F,
                (req.start_address >> 8) as u8,
                (req.start_address & 0xFF) as u8,
                (req.quantity >> 8) as u8,
                (req.quantity & 0xFF) as u8,
                byte_count,
            ];
            for i in 0..byte_count as usize {
                pdu.push(if i < values.len() { values[i] as u8 } else { 0 });
            }
            Ok(pdu)
        }
        0x10 => {
            let values = req.write_values.as_deref().unwrap_or(&[]);
            let qty = values.len() as u16;
            let byte_count = (qty * 2) as u8;
            let mut pdu = vec![
                0x10,
                (req.start_address >> 8) as u8,
                (req.start_address & 0xFF) as u8,
                (qty >> 8) as u8,
                (qty & 0xFF) as u8,
                byte_count,
            ];
            for &v in values {
                pdu.push((v >> 8) as u8);
                pdu.push((v & 0xFF) as u8);
            }
            Ok(pdu)
        }
        _ => Err(format!("不支持的功能码: 0x{:02X}", req.function_code)),
    }
}

// RTU frame = [slave_id] + PDU + [CRC lo] + [CRC hi]
fn build_rtu_frame(slave_id: u8, pdu: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(1 + pdu.len() + 2);
    frame.push(slave_id);
    frame.extend_from_slice(pdu);
    append_crc(&mut frame);
    frame
}

// TCP frame = MBAP header + PDU
// MBAP: [transaction_id(2)] [protocol_id(2)=0x0000] [length(2)] [unit_id(1)]
fn build_tcp_frame(transaction_id: u16, unit_id: u8, pdu: &[u8]) -> Vec<u8> {
    let length = (pdu.len() + 1) as u16; // PDU + unit_id
    let mut frame = Vec::with_capacity(7 + pdu.len());
    frame.push((transaction_id >> 8) as u8);
    frame.push((transaction_id & 0xFF) as u8);
    frame.push(0x00); // protocol ID hi
    frame.push(0x00); // protocol ID lo
    frame.push((length >> 8) as u8);
    frame.push((length & 0xFF) as u8);
    frame.push(unit_id);
    frame.extend_from_slice(pdu);
    frame
}

// ---- Response parsing ----

fn exception_text(code: u8) -> String {
    match code {
        0x01 => "非法功能码".to_string(),
        0x02 => "非法数据地址".to_string(),
        0x03 => "非法数据值".to_string(),
        0x04 => "从站设备故障".to_string(),
        0x05 => "确认（处理中）".to_string(),
        0x06 => "从站设备忙".to_string(),
        0x07 => "否定确认".to_string(),
        0x08 => "内存奇偶校验错误".to_string(),
        0x0A => "网关路径不可用".to_string(),
        0x0B => "网关目标设备无响应".to_string(),
        _ => format!("未知异常码 0x{:02X}", code),
    }
}

fn parse_response_data(fc: u8, data: &[u8]) -> Vec<u16> {
    match fc {
        0x01 | 0x02 => {
            if data.is_empty() { return vec![]; }
            data[1..].iter().map(|&b| b as u16).collect()
        }
        0x03 | 0x04 => {
            if data.is_empty() { return vec![]; }
            data[1..].chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| ((c[0] as u16) << 8) | (c[1] as u16))
                .collect()
        }
        0x05 | 0x06 => {
            if data.len() >= 4 {
                vec![((data[2] as u16) << 8) | (data[3] as u16)]
            } else { vec![] }
        }
        0x0F | 0x10 => {
            if data.len() >= 4 {
                vec![
                    ((data[0] as u16) << 8) | (data[1] as u16),
                    ((data[2] as u16) << 8) | (data[3] as u16),
                ]
            } else { vec![] }
        }
        _ => vec![],
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---- RTU query logic ----

fn rtu_query(port: &mut Box<dyn serialport::SerialPort>, request: &ModbusRequest) -> Result<ModbusResponse, String> {
    let pdu = build_pdu(request)?;
    let request_frame = build_rtu_frame(request.slave_id, &pdu);
    let start = now_ms();

    let _ = port.clear(serialport::ClearBuffer::Input);
    port.write_all(&request_frame).map_err(|e| format!("[E2002] RTU 发送失败: {}", e))?;
    port.flush().map_err(|e| format!("[E2003] RTU 刷新缓冲区失败: {}", e))?;

    let mut buf = vec![0u8; 256];
    let mut total = 0;

    loop {
        match port.read(&mut buf[total..]) {
            Ok(n) => {
                total += n;
                if total >= 5 {
                    if buf[1] & 0x80 != 0 { break; }
                    if matches!(request.function_code, 0x01 | 0x02 | 0x03 | 0x04) {
                        let expected = 3 + buf[2] as usize + 2;
                        if total >= expected { break; }
                    }
                    if matches!(request.function_code, 0x05 | 0x06 | 0x0F | 0x10) && total >= 8 {
                        break;
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                if total > 0 { break; }
                let elapsed = now_ms() - start;
                return Err(format!("[E2004] RTU 响应超时 ({}ms)，从站 {} 无响应。请检查: 1)从站地址是否正确 2)波特率/校验位是否匹配 3)接线是否正常", elapsed, request.slave_id));
            }
            Err(e) => return Err(format!("[E2005] RTU 读取失败: {}。请检查串口连接是否断开", e)),
        }
    }

    let elapsed = now_ms() - start;
    let raw_response = buf[..total].to_vec();
    let crc_ok = verify_crc(&raw_response);

    if total >= 5 && raw_response[1] & 0x80 != 0 {
        let exc = raw_response[2];
        return Ok(ModbusResponse {
            ts: now_ms(), slave_id: raw_response[0], function_code: raw_response[1],
            is_exception: true, exception_code: Some(exc), exception_text: Some(exception_text(exc)),
            data: vec![], raw_request: request_frame, raw_response, crc_ok, elapsed_ms: elapsed,
            mode: "RTU".to_string(), slow: elapsed > 1000,
        });
    }

    let data_portion = if total > 4 { &raw_response[2..total - 2] } else { &[] as &[u8] };
    let data = parse_response_data(request.function_code, data_portion);

    Ok(ModbusResponse {
     ts: now_ms(), slave_id: raw_response[0], function_code: request.function_code,
        is_exception: false, exception_code: None, exception_text: None,
        data, raw_request: request_frame, raw_response, crc_ok, elapsed_ms: elapsed,
        mode: "RTU".to_string(), slow: elapsed > 1000,
    })
}

// ---- TCP query logic ----

fn tcp_query(stream: &mut TcpStream, transaction_id: u16, request: &ModbusRequest) -> Result<ModbusResponse, String> {
    let pdu = build_pdu(request)?;
    let request_frame = build_tcp_frame(transaction_id, request.slave_id, &pdu);
    let start = now_ms();

    stream.write_all(&request_frame).map_err(|e| format!("[E3001] TCP 发送失败: {}。连接可能已断开", e))?;
    stream.flush().map_err(|e| format!("[E3002] TCP 刷新失败: {}", e))?;

    // Read MBAP header (7 bytes)
    let mut header = [0u8; 7];
    stream.read_exact(&mut header).map_err(|e| {
        if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock {
            format!("[E3003] TCP 响应超时，从站 {} 无响应。请检查: 1)IP和端口是否正确 2)设备是否在线 3)从站地址是否匹配", request.slave_id)
        } else {
            format!("[E3004] TCP 读取MBAP头失败: {}。连接可能已断开", e)
        }
    })?;

    let resp_length = ((header[4] as usize) << 8) | (header[5] as usize);
    if resp_length < 2 || resp_length > 253 {
        return Err(format!("[E3005] 无效的 MBAP 长度: {}，有效范围 2-253", resp_length));
    }

    // We already have unit_id in header[6], read remaining PDU bytes
    let pdu_len = resp_length - 1; // subtract unit_id
    let mut pdu_buf = vec![0u8; pdu_len];
    stream.read_exact(&mut pdu_buf).map_err(|e| format!("[E3006] TCP PDU 读取失败: {}。响应数据不完整", e))?;

    let elapsed = now_ms() - start;

    // Build full raw response (MBAP header + PDU) for display
    let mut raw_response = header.to_vec();
    raw_response.extend_from_slice(&pdu_buf);

    let unit_id = header[6];
    let resp_fc = pdu_buf[0];

    // Check exception
    if resp_fc & 0x80 != 0 {
        let exc = if pdu_buf.len() > 1 { pdu_buf[1] } else { 0 };
        return Ok(ModbusResponse {
            ts: now_ms(), slave_id: unit_id, function_code: resp_fc,
            is_exception: true, exception_code: Some(exc), exception_text: Some(exception_text(exc)),
            data: vec![], raw_request: request_frame, raw_response, crc_ok: true, elapsed_ms: elapsed,
            mode: "TCP".to_string(), slow: elapsed > 1000,
        });
    }

    // Parse data from PDU (skip function code byte)
    let data_portion = &pdu_buf[1..];
    let data = parse_tcp_response_data(request.function_code, data_portion);

    Ok(ModbusResponse {
        ts: now_ms(), slave_id: unit_id, function_code: request.function_code,
        is_exception: false, exception_code: None, exception_text: None,
        data, raw_request: request_frame, raw_response, crc_ok: true, elapsed_ms: elapsed,
        mode: "TCP".to_string(), slow: elapsed > 1000,
    })
}

fn parse_tcp_response_data(fc: u8, data: &[u8]) -> Vec<u16> {
    match fc {
        0x01 | 0x02 => {
            if data.is_empty() { return vec![]; }
            let _byte_count = data[0];
            data[1..].iter().map(|&b| b as u16).collect()
        }
        0x03 | 0x04 => {
            if data.is_empty() { return vec![]; }
            let _byte_count = data[0];
            data[1..].chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| ((c[0] as u16) << 8) | (c[1] as u16))
                .collect()
        }
        0x05 | 0x06 => {
            if data.len() >= 4 {
                vec![((data[2] as u16) << 8) | (data[3] as u16)]
            } else { vec![] }
        }
        0x0F | 0x10 => {
            if data.len() >= 4 {
                vec![
                    ((data[0] as u16) << 8) | (data[1] as u16),
                    ((data[2] as u16) << 8) | (data[3] as u16),
                ]
            } else { vec![] }
        }
        _ => vec![],
    }
}

// ---- Frame detection & monitor ----

fn calc_t35_us(baud_rate: u32) -> u64 {
    let char_us = 11_000_000u64 / baud_rate as u64;
    let t35 = char_us * 35 / 10;
    t35.max(1750)
}

/// 根据功能码预测 RTU 帧长度 (含 slave_id + fc + data + CRC)
fn expected_rtu_frame_len(raw: &[u8]) -> Option<usize> {
    if raw.len() < 2 { return None; }
    let fc = raw[1];
    if fc & 0x80 != 0 { return Some(5); }
    match fc {
        0x01 | 0x02 | 0x03 | 0x04 => {
            if raw.len() >= 3 {
                let resp_len = 3 + raw[2] as usize + 2;
                if raw.len() >= resp_len && resp_len >= 5 { Some(resp_len) } else { Some(8) }
            } else { None }
        }
        0x05 | 0x06 => Some(8),
        0x0F | 0x10 => {
            if raw.len() >= 7 { Some(7 + raw[6] as usize + 2) } else { Some(8) }
        }
        _ => None,
    }
}

/// 候选帧评分: CRC有效+长度匹配+从站地址合理 = 高分
fn score_candidate(raw: &[u8]) -> u32 {
    if raw.len() < 4 { return 0; }
    if !verify_crc(raw) { return 0; }
    let mut score = 10u32; // CRC 有效
    if raw[0] >= 1 && raw[0] <= 247 { score += 3; }
    let fc = raw[1] & 0x7F;
    if matches!(fc, 0x01..=0x06 | 0x0F | 0x10) { score += 3; }
    if let Some(expected) = expected_rtu_frame_len(raw) {
        if raw.len() == expected { score += 5; }
    }
    score
}

/// 滑动窗口提取帧: buffer 可能含多帧或垃圾数据
fn extract_frames(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut offset = 0;
    while offset < buf.len().saturating_sub(3) {
        let mut best_score = 0u32;
        let mut best_end = 0usize;
        for end in (offset + 4)..=buf.len().min(offset + 256) {
            let s = score_candidate(&buf[offset..end]);
            if s > best_score { best_score = s; best_end = end; }
        }
        if best_score >= 13 {
            frames.push(buf[offset..best_end].to_vec());
            offset = best_end;
        } else {
            offset += 1;
        }
    }
    frames
}

fn parse_monitor_frame(raw: &[u8], ts: u64) -> Option<MonitorFrame> {
    if raw.len() < 4 { return None; } // min: slave(1) + fc(1) + crc(2)
    let crc_ok = verify_crc(raw);
    let slave_id = raw[0];
    let fc = raw[1];
    let is_exception = fc & 0x80 != 0;

    if is_exception {
        let exc = if raw.len() > 2 { raw[2] } else { 0 };
        return Some(MonitorFrame {
            ts, raw: raw.to_vec(), slave_id, function_code: fc,
            is_exception: true, exception_code: Some(exc),
            exception_text: Some(exception_text(exc)),
            data: vec![], crc_ok,
        });
    }

    let data_portion = if raw.len() > 4 { &raw[2..raw.len() - 2] } else { &[] as &[u8] };
    let data = parse_response_data(fc & 0x7F, data_portion);

    Some(MonitorFrame {
        ts, raw: raw.to_vec(), slave_id, function_code: fc,
        is_exception: false, exception_code: None, exception_text: None,
        data, crc_ok,
    })
}

fn monitor_loop(
    mut port: Box<dyn serialport::SerialPort>,
    baud_rate: u32,
    running: Arc<AtomicBool>,
    app: AppHandle,
) {
    let t35 = Duration::from_micros(calc_t35_us(baud_rate));
    let _ = port.set_timeout(Duration::from_millis(50));
    let mut buf = [0u8; 512];
    let mut frame_buf: Vec<u8> = Vec::with_capacity(256);
    let mut last_byte_time = Instant::now();
    let mut pair_state = PairState { pending: None, timeout_history: Vec::new() };

    while running.load(Ordering::Relaxed) {
        match port.read(&mut buf) {
            Ok(n) if n > 0 => {
                let now = Instant::now();
                if !frame_buf.is_empty() && now.duration_since(last_byte_time) > t35 {
                    process_frame(&frame_buf, &mut pair_state, &app);
                    frame_buf.clear();
                }
                frame_buf.extend_from_slice(&buf[..n]);
                last_byte_time = Instant::now();
            }
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                if !frame_buf.is_empty() {
                    process_frame(&frame_buf, &mut pair_state, &app);
                    frame_buf.clear();
                }
                // 检查 pending 是否超时
                if let Some(ref req) = pair_state.pending {
                    if now_ms().saturating_sub(req.ts) > PAIR_TIMEOUT_MS {
                        let timed_out = pair_state.pending.take().unwrap();
                        let orphan = MonitorPair { request: Some(timed_out.clone()), response: None, elapsed_ms: 0, retry_hint: false };
                        let _ = app.emit("monitor-frame", orphan);
                        pair_state.timeout_history.push(timed_out);
                        if pair_state.timeout_history.len() > MAX_TIMEOUT_HISTORY {
                            pair_state.timeout_history.remove(0);
                        }
                    }
                }
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    if !frame_buf.is_empty() {
        process_frame(&frame_buf, &mut pair_state, &app);
    }
    if let Some(req) = pair_state.pending.take() {
        let pair = MonitorPair { request: Some(req), response: None, elapsed_ms: 0, retry_hint: false };
        let _ = app.emit("monitor-frame", pair);
    }
}

fn process_frame(raw: &[u8], state: &mut PairState, app: &AppHandle) {
    let frames_raw = if raw.len() >= 4 && verify_crc(raw) {
        vec![raw.to_vec()]
    } else {
        extract_frames(raw)
    };
    for frame_raw in frames_raw {
        let ts = now_ms();
        let frame = match parse_monitor_frame(&frame_raw, ts) {
            Some(f) => f,
            None => continue,
        };
        pair_frame(frame, state, app);
    }
}

fn pair_frame(frame: MonitorFrame, state: &mut PairState, app: &AppHandle) {
    // 1. 尝试匹配当前 pending 请求
    if let Some(req) = state.pending.as_ref() {
        if frame.slave_id == req.slave_id {
            let elapsed = frame.ts.saturating_sub(req.ts);
            let pair = MonitorPair {
                request: Some(req.clone()),
                response: Some(frame),
                elapsed_ms: elapsed,
                retry_hint: false,
            };
            let _ = app.emit("monitor-frame", pair);
            state.pending = None;
            return;
        }
    }

    // 2. 尝试从超时历史中回溯匹配（迟到响应）
    if let Some(idx) = state.timeout_history.iter().rposition(|r| r.slave_id == frame.slave_id) {
        let old_req = state.timeout_history.remove(idx);
        let elapsed = frame.ts.saturating_sub(old_req.ts);
        let pair = MonitorPair {
            request: Some(old_req),
            response: Some(frame),
            elapsed_ms: elapsed,
            retry_hint: true, // 标记为可能的重试/迟到响应
        };
        let _ = app.emit("monitor-frame", pair);
        return;
    }

    // 3. 无匹配，当前 pending 变孤立，新帧成为 pending
    if let Some(old) = state.pending.take() {
        let orphan = MonitorPair { request: Some(old), response: None, elapsed_ms: 0, retry_hint: false };
        let _ = app.emit("monitor-frame", orphan);
    }
    state.pending = Some(frame);
}

// ---- Tauri commands ----

#[tauri::command]
fn list_ports() -> Result<Vec<PortInfo>, String> {
    let ports = serialport::available_ports().map_err(|e| format!("[E5001] 获取串口列表失败: {}。请检查系统串口驱动", e))?;
    Ok(ports
        .into_iter()
        .map(|p| PortInfo {
            name: p.port_name,
            port_type: match p.port_type {
                serialport::SerialPortType::UsbPort(info) => {
                    format!("USB ({} {})",
                        info.manufacturer.unwrap_or_default(),
                        info.product.unwrap_or_default())
                }
                serialport::SerialPortType::BluetoothPort => "Bluetooth".to_string(),
                serialport::SerialPortType::PciPort => "PCI".to_string(),
                serialport::SerialPortType::Unknown => "Unknown".to_string(),
            },
        })
        .collect())
}

#[tauri::command]
fn connect_port(
    state: State<'_, AppState>,
    port_name: String,
    config: SerialConfig,
) -> Result<(), String> {
    let mut conn = state.connection.lock().unwrap();
    if conn.is_some() {
        return Err("[E2010] 已有连接，请先断开当前连接".to_string());
    }

    let data_bits = match config.data_bits {
        7 => serialport::DataBits::Seven,
        8 => serialport::DataBits::Eight,
        _ => return Err(format!("[E2011] 不支持的数据位: {}，仅支持 7 或 8", config.data_bits)),
    };
    let parity = match config.parity.as_str() {
        "None" => serialport::Parity::None,
        "Even" => serialport::Parity::Even,
        "Odd" => serialport::Parity::Odd,
        _ => return Err(format!("[E2012] 不支持的校验位: {}，仅支持 None/Even/Odd", config.parity)),
    };
    let stop_bits = match config.stop_bits {
        1 => serialport::StopBits::One,
        2 => serialport::StopBits::Two,
        _ => return Err(format!("[E2013] 不支持的停止位: {}，仅支持 1 或 2", config.stop_bits)),
    };

    let port = serialport::new(&port_name, config.baud_rate)
        .data_bits(data_bits)
        .parity(parity)
        .stop_bits(stop_bits)
        .timeout(Duration::from_millis(1000))
        .open()
        .map_err(|e| format!("[E2014] 无法打开串口 {}: {}。请检查串口是否被占用或设备是否连接", port_name, e))?;

    *conn = Some(Connection::Serial(port));
    Ok(())
}

#[tauri::command]
fn connect_tcp(
    state: State<'_, AppState>,
    config: TcpConfig,
) -> Result<(), String> {
    let mut conn = state.connection.lock().unwrap();
    if conn.is_some() {
        return Err("[E3010] 已有连接，请先断开当前连接".to_string());
    }

    let addr = format!("{}:{}", config.ip, config.port);
    let stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|e| format!("[E3011] 无效地址 {}: {}。请检查 IP 格式", addr, e))?,
        Duration::from_secs(5),
    ).map_err(|e| format!("[E3012] TCP 连接失败 {}: {}。请检查: 1)IP和端口是否正确 2)设备是否在线 3)防火墙设置", addr, e))?;

    stream.set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| format!("[E3013] 设置读超时失败: {}", e))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| format!("[E3014] 设置写超时失败: {}", e))?;
    stream.set_nodelay(true)
        .map_err(|e| format!("[E3015] 设置 TCP_NODELAY 失败: {}", e))?;

    *conn = Some(Connection::Tcp(stream));
    *state.tcp_transaction_id.lock().unwrap() = 0;
    Ok(())
}

#[tauri::command]
fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    let mut conn = state.connection.lock().unwrap();
    if conn.is_none() {
        return Err("[E4001] 当前无活动连接".to_string());
    }
    *conn = None;
    Ok(())
}

#[tauri::command]
fn export_log(content: String, format: String) -> Result<String, String> {
    let ext = match format.as_str() {
        "csv" => "csv",
        "json" => "json",
        _ => return Err("[E6001] 不支持的导出格式，仅支持 csv/json".to_string()),
    };
    let dialog = rfd::FileDialog::new()
        .set_title("导出通信记录")
        .add_filter(
            if ext == "csv" { "CSV 文件" } else { "JSON 文件" },
            &[ext],
        )
        .set_file_name(format!("modbus_log.{}", ext))
        .save_file();

    match dialog {
        Some(path) => {
            std::fs::write(&path, content.as_bytes())
                .map_err(|e| format!("[E6002] 写入文件失败: {}。路径: {:?}", e, path))?;
            Ok(path.to_string_lossy().to_string())
        }
        None => Err("[E6003] 用户取消了导出".to_string()),
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct RegisterMap {
    address: u16,
    name: String,
    unit: String,
    scale: f64,
}

#[tauri::command]
fn save_register_map(app: AppHandle, maps: Vec<RegisterMap>) -> Result<(), String> {
    let dir = app.path().app_data_dir().map_err(|e| format!("[E8001] 获取数据目录失败: {}", e))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("[E8002] 创建目录失败: {}", e))?;
    let path = dir.join("register_map.json");
    let json = serde_json::to_string_pretty(&maps).map_err(|e| format!("[E8003] 序列化失败: {}", e))?;
    std::fs::write(&path, json).map_err(|e| format!("[E8004] 写入失败: {}", e))?;
    Ok(())
}

#[tauri::command]
fn load_register_map(app: AppHandle) -> Result<Vec<RegisterMap>, String> {
    let dir = app.path().app_data_dir().map_err(|e| format!("[E8005] 获取数据目录失败: {}", e))?;
    let path = dir.join("register_map.json");
    if !path.exists() { return Ok(vec![]); }
    let json = std::fs::read_to_string(&path).map_err(|e| format!("[E8006] 读取失败: {}", e))?;
    serde_json::from_str(&json).map_err(|e| format!("[E8007] 解析失败: {}", e))
}

#[derive(Deserialize)]
struct AiConfig {
    api_url: String,
    api_key: String,
    model: String,
}

#[tauri::command]
fn ai_analyze(config: AiConfig, context: String) -> Result<String, String> {
    let prompt = format!(
        "你是一个 Modbus 通信诊断专家。请分析以下 Modbus 通信记录，指出异常模式、潜在问题和优化建议。用中文回答，简洁明了。\n\n{}", context
    );
    let body = serde_json::json!({
        "model": config.model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 2000,
    });
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("[E9001] 创建 HTTP 客户端失败: {}", e))?;
    let resp = client.post(&config.api_url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| format!("[E9002] API 请求失败: {}。请检查网络和 API 地址", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        return Err(format!("[E9003] API 返回错误 {}: {}", status, text));
    }
    let json: serde_json::Value = resp.json()
        .map_err(|e| format!("[E9004] 解析 API 响应失败: {}", e))?;
    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("AI 未返回有效内容")
        .to_string();
    Ok(content)
}

#[tauri::command]
fn save_ai_config(app: AppHandle, config: AiConfig) -> Result<(), String> {
    let dir = app.path().app_data_dir().map_err(|e| format!("[E9005] {}", e))?;
    std::fs::create_dir_all(&dir).ok();
    let data = serde_json::json!({"api_url": config.api_url, "model": config.model, "api_key": config.api_key});
    std::fs::write(dir.join("ai_config.json"), data.to_string())
        .map_err(|e| format!("[E9006] 保存配置失败: {}", e))
}

#[tauri::command]
fn load_ai_config(app: AppHandle) -> Result<serde_json::Value, String> {
    let dir = app.path().app_data_dir().map_err(|e| format!("[E9007] {}", e))?;
    let path = dir.join("ai_config.json");
    if !path.exists() { return Ok(serde_json::json!({"api_url":"","model":"","api_key":""})); }
    let json = std::fs::read_to_string(&path).map_err(|e| format!("[E9008] {}", e))?;
    serde_json::from_str(&json).map_err(|e| format!("[E9009] {}", e))
}

#[tauri::command]
fn start_monitor(
    state: State<'_, AppState>,
    app: AppHandle,
    baud_rate: u32,
) -> Result<(), String> {
    if state.monitor_running.load(Ordering::Relaxed) {
        return Err("[E7001] 监听已在运行中".to_string());
    }
    let mut conn = state.connection.lock().unwrap();
    let port = match conn.take() {
        Some(Connection::Serial(p)) => p,
        Some(other) => {
            *conn = Some(other);
            return Err("[E7002] 被动监听仅支持 RTU 串口模式".to_string());
        }
        None => return Err("[E7003] 请先连接串口".to_string()),
    };

    let running = state.monitor_running.clone();
    running.store(true, Ordering::Relaxed);

    let handle = std::thread::spawn(move || {
        monitor_loop(port, baud_rate, running, app);
    });

    *state.monitor_thread.lock().unwrap() = Some(handle);
    Ok(())
}

#[tauri::command]
fn stop_monitor(state: State<'_, AppState>) -> Result<(), String> {
    if !state.monitor_running.load(Ordering::Relaxed) {
        return Err("[E7004] 监听未在运行".to_string());
    }
    state.monitor_running.store(false, Ordering::Relaxed);
    if let Some(handle) = state.monitor_thread.lock().unwrap().take() {
        let _ = handle.join();
    }
    // 注意: 监听结束后串口已被消费，需要重新连接
    Ok(())
}

#[tauri::command]
fn modbus_query(
    state: State<'_, AppState>,
    request: ModbusRequest,
) -> Result<ModbusResponse, String> {
    // E1001: 从站地址校验
    if request.slave_id == 0 || request.slave_id > 247 {
        return Err(format!("[E1001] 从站地址无效: {}，有效范围 1-247", request.slave_id));
    }
    // E1002: 功能码校验
    if !matches!(request.function_code, 0x01..=0x06 | 0x0F | 0x10) {
        return Err(format!("[E1002] 不支持的功能码: 0x{:02X}", request.function_code));
    }
    // E1003: 数量校验
    match request.function_code {
        0x01 | 0x02 => if request.quantity == 0 || request.quantity > 2000 {
            return Err(format!("[E1003] 线圈数量无效: {}，有效范围 1-2000", request.quantity));
        },
        0x03 | 0x04 => if request.quantity == 0 || request.quantity > 125 {
            return Err(format!("[E1003] 寄存器数量无效: {}，有效范围 1-125", request.quantity));
        },
        0x0F => if request.quantity == 0 || request.quantity > 1968 {
            return Err(format!("[E1003] 写线圈数量无效: {}，有效范围 1-1968", request.quantity));
        },
        0x10 => if request.quantity == 0 || request.quantity > 123 {
            return Err(format!("[E1003] 写寄存器数量无效: {}，有效范围 1-123", request.quantity));
        },
        _ => {}
    }
    // E1004: 地址+数量溢出校验
    if (request.start_address as u32) + (request.quantity as u32) > 65536 {
        return Err(format!("[E1004] 地址范围溢出: 起始地址 {} + 数量 {} > 65535",
            request.start_address, request.quantity));
    }
    // E1005: 写操作必须提供值
    if matches!(request.function_code, 0x05 | 0x06 | 0x0F | 0x10) {
        if request.write_values.as_ref().map_or(true, |v| v.is_empty()) {
            return Err("[E1005] 写操作必须提供写入值".to_string());
        }
    }

    let mut conn = state.connection.lock().unwrap();
    let connection = conn.as_mut().ok_or("[E2001] 未建立连接，请先连接串口或TCP")?;

    match connection {
        Connection::Serial(port) => rtu_query(port, &request),
        Connection::Tcp(stream) => {
            let mut tid = state.tcp_transaction_id.lock().unwrap();
            *tid = tid.wrapping_add(1);
            let transaction_id = *tid;
            tcp_query(stream, transaction_id, &request)
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            list_ports,
            connect_port,
            connect_tcp,
            disconnect,
            export_log,
            save_register_map,
            load_register_map,
            ai_analyze,
            save_ai_config,
            load_ai_config,
            start_monitor,
            stop_monitor,
            modbus_query,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
