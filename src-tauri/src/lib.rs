use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, State};

#[derive(Serialize, Clone)]
struct PortInfo {
    name: String,
    port_type: String,
}

#[derive(serde::Deserialize, Clone)]
struct SerialConfig {
    baud_rate: u32,
    data_bits: u8,
    parity: String,
    stop_bits: u8,
}

struct SerialState {
    running: Arc<AtomicBool>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Default for SerialState {
    fn default() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
        }
    }
}

#[tauri::command]
fn list_ports() -> Result<Vec<PortInfo>, String> {
    let ports = serialport::available_ports().map_err(|e| e.to_string())?;
    Ok(ports
        .into_iter()
        .map(|p| PortInfo {
            name: p.port_name,
            port_type: match p.port_type {
                serialport::SerialPortType::UsbPort(info) => {
                    format!(
                        "USB ({} {})",
                        info.manufacturer.unwrap_or_default(),
                        info.product.unwrap_or_default()
                    )
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
    app: AppHandle,
    state: State<'_, SerialState>,
    port_name: String,
    config: SerialConfig,
) -> Result<(), String> {
    if state.running.load(Ordering::Relaxed) {
        return Err("已有连接，请先断开".to_string());
    }

    let data_bits = match config.data_bits {
        7 => serialport::DataBits::Seven,
        8 => serialport::DataBits::Eight,
        _ => return Err(format!("不支持的数据位: {}", config.data_bits)),
    };

    let parity = match config.parity.as_str() {
        "None" => serialport::Parity::None,
        "Even" => serialport::Parity::Even,
        "Odd" => serialport::Parity::Odd,
        _ => return Err(format!("不支持的校验位: {}", config.parity)),
    };

    let stop_bits = match config.stop_bits {
        1 => serialport::StopBits::One,
        2 => serialport::StopBits::Two,
        _ => return Err(format!("不支持的停止位: {}", config.stop_bits)),
    };

    let port = serialport::new(&port_name, config.baud_rate)
        .data_bits(data_bits)
        .parity(parity)
        .stop_bits(stop_bits)
        .timeout(Duration::from_millis(100))
        .open()
        .map_err(|e| format!("无法打开串口 {}: {}", port_name, e))?;

    state.running.store(true, Ordering::Relaxed);
    let running = state.running.clone();

    let handle = std::thread::spawn(move || {
        let mut port = port;
        let mut buf = [0u8; 1024];

        while running.load(Ordering::Relaxed) {
            match port.read(&mut buf) {
                Ok(n) if n > 0 => {
                    let ts = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let data: Vec<u8> = buf[..n].to_vec();
                    let _ = app.emit("serial-data", serde_json::json!({ "ts": ts, "data": data }));
                }
                Ok(_) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    let _ = app.emit(
                        "serial-error",
                        serde_json::json!({ "error": format!("串口读取错误: {}", e) }),
                    );
                    break;
                }
            }
        }
    });

    *state.thread.lock().unwrap() = Some(handle);
    Ok(())
}

#[tauri::command]
fn disconnect_port(state: State<'_, SerialState>) -> Result<(), String> {
    state.running.store(false, Ordering::Relaxed);
    if let Some(handle) = state.thread.lock().unwrap().take() {
        handle.join().map_err(|_| "线程退出异常".to_string())?;
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(SerialState::default())
        .invoke_handler(tauri::generate_handler![
            list_ports,
            connect_port,
            disconnect_port,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
