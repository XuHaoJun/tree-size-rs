use dashmap::DashMap;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::fs; // 替换标准库 fs
use kanal;

/// Represents size information for a file or directory
#[derive(Clone, Debug)]
struct FileSystemEntry {
    /// Path to the file or directory
    path: PathBuf,
    /// Size in bytes
    size_bytes: u64,
}

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
async fn print_tree_size(path: &str) -> Result<String, ()> {
    let _ = run_tree_size(path.to_string()).await;
    Ok(format!("Hello, {}! You've been greeted from Rust!", path))
}

async fn run_tree_size(path: String) -> std::io::Result<()> {
    let target_dir = Path::new(&path).canonicalize()?;
    let size_map = Arc::new(DashMap::new());

    // 异步预热父目录链
    let path_chain = get_parent_chain(&target_dir).await;
    for p in path_chain {
        size_map.entry(p).or_insert(Arc::new(AtomicU64::new(0)));
    }

    // Create a channel with capacity 100 for sending file system entries
    let (sender, receiver) = kanal::bounded::<FileSystemEntry>(100);
    
    // Spawn a task to process received entries
    let receiver_handle = tokio::spawn(async move {
        while let Ok(entry) = receiver.recv() {
            println!("Path: {}, Size: {} bytes", entry.path.display(), entry.size_bytes);
        }
    });

    calculate_size_async(target_dir, size_map.clone(), Some(sender)).await?;
    
    // Close the channel
    // The sender will be dropped after the calculation is done

    // Wait for receiver task to complete
    let _ = receiver_handle.await;

    Ok(())
}

// 异步递归处理函数
async fn calculate_size_async(
    path: PathBuf,
    size_map: Arc<DashMap<PathBuf, Arc<AtomicU64>>>,
    sender: Option<kanal::Sender<FileSystemEntry>>,
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
                updates.push((e.get().clone(), self_size, path.clone()));
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                let counter = Arc::new(AtomicU64::new(self_size));
                updates.push((counter.clone(), 0, path.clone()));
                e.insert(counter);
            }
        }
    }

    // 父目录链更新 - 使用 Rayon 并行处理
    let parent_chain = get_parent_chain_sync(&path);
    parent_chain.par_iter().for_each(|parent| {
        if let Some(counter) = size_map.get(parent) {
            counter.fetch_add(self_size, Ordering::Relaxed);
            let value = counter.load(Ordering::Relaxed);
            
            // Send parent path and value to channel if available
            if let Some(sender) = &sender {
                let _ = sender.send(FileSystemEntry {
                    path: parent.clone(),
                    size_bytes: value,
                });
            }
        }
    });

    // 直接处理更新，不使用 thread_local
    for (counter, size, path) in updates {
        counter.fetch_add(size, Ordering::Relaxed);
        let value = counter.load(Ordering::Relaxed);
        
        // Send path and value to channel if available
        if let Some(sender) = &sender {
            let _ = sender.send(FileSystemEntry {
                path: path,
                size_bytes: value,
            });
        }
    }

    // 异步处理子目录
    if metadata.is_dir() {
        let mut entries = Vec::new();
        let mut children = fs::read_dir(&path).await?;

        // 收集所有子目录条目
        while let Some(entry) = children.next_entry().await? {
            entries.push(entry.path());
        }

        // 使用 Rayon 并行处理子目录
        let results: Vec<std::io::Result<()>> = entries
            .par_iter()
            .map(|child_path| {
                // 注意：这里需要使用 tokio::runtime::Handle 来运行异步任务
                let size_map_clone = size_map.clone();
                let child_path_clone = child_path.clone();
                let sender_clone = sender.clone();

                // 创建一个同步的阻塞任务，内部运行异步代码
                tokio::task::block_in_place(|| {
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(calculate_size_async(child_path_clone, size_map_clone, sender_clone))
                })
            })
            .collect();

        // 检查结果中的错误
        for result in results {
            result?;
        }
    }

    Ok(())
}

// 同步版本的获取父目录链函数，用于 Rayon 并行处理
fn get_parent_chain_sync(path: &Path) -> Vec<PathBuf> {
    let mut chain = vec![];
    let mut current = path;
    while let Some(parent) = current.parent() {
        chain.push(parent.to_path_buf());
        current = parent;
    }
    chain
}

// 异步获取父目录链
async fn get_parent_chain(path: &Path) -> Vec<PathBuf> {
    get_parent_chain_sync(path)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![print_tree_size])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
