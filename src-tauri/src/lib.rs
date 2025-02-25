use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use dashmap::DashMap;
use tokio::fs; // 替换标准库 fs

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

async fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let target_dir = Path::new(&path).canonicalize()?;
    let size_map = Arc::new(DashMap::new());

    // 异步预热父目录链
    let path_chain = get_parent_chain(&target_dir).await;
    for p in path_chain {
        size_map.entry(p).or_insert(Arc::new(AtomicU64::new(0)));
    }

    calculate_size_async(target_dir, size_map.clone()).await?;

    // 输出逻辑保持不变...
    Ok(())
}

// 异步递归处理函数
async fn calculate_size_async(
    path: PathBuf,
    size_map: Arc<DashMap<PathBuf, Arc<AtomicU64>>>,
) -> std::io::Result<()> {
    if path.is_symlink() {
        return Ok(());
    }

    // 异步获取元数据
    let metadata = match fs::metadata(&path).await {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };

    let self_size = metadata.len();

    // 移除 thread_local! 代码块，直接处理更新
    let mut updates = vec![];
    if metadata.is_dir() {
        match size_map.entry(path.clone()) {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                updates.push((e.get().clone(), self_size))
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                let counter = Arc::new(AtomicU64::new(self_size));
                updates.push((counter.clone(), 0));
                e.insert(counter);
            }
        }
    }

    // 父目录链更新
    let mut current_parent = path.parent();
    while let Some(parent) = current_parent {
        if let Some(counter) = size_map.get(parent) {
            updates.push((counter.clone(), self_size));
        }
        current_parent = parent.parent();
    }

    // 直接处理更新，不使用 thread_local
    for (counter, size) in updates {
        counter.fetch_add(size, Ordering::Relaxed);
    }

    // 异步处理子目录
    if metadata.is_dir() {
        let mut children = fs::read_dir(&path).await?;
        
        while let Some(entry) = children.next_entry().await? {
            let child_path = entry.path();
            
            // Box the recursive future call to handle infinitely sized futures
            Box::pin(calculate_size_async(child_path, size_map.clone())).await?;
        }
    }

    Ok(())
}

// 异步获取父目录链
async fn get_parent_chain(path: &Path) -> Vec<PathBuf> {
    let mut chain = vec![];
    let mut current = path;
    while let Some(parent) = current.parent() {
        chain.push(parent.to_path_buf());
        current = parent;
    }
    chain
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![greet])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
