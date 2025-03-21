# 效能優化摘要

## 小目錄使用單線程處理

我們針對小型目錄（少於 20 個項目）改為使用單線程處理，以減少 Rayon 的調度開銷：

```rust
// Process children based on directory size
if sorted_entries.len() < 20 {
  // Use sequential processing for very small directories
  for child_path in &sorted_entries {
    let _ = calculate_size_sync(
      child_path.as_path(),
      analytics_map.clone(),
      target_dir_path,
      visited_inodes.clone(),
      processed_paths.clone(),
    );
  }
} else {
  // Use parallel processing for larger directories
  sorted_entries.par_iter().for_each(|child_path| {
    let _ = calculate_size_sync(
      child_path.as_path(),
      analytics_map.clone(),
      target_dir_path,
      visited_inodes.clone(),
      processed_paths.clone(),
    );
  });
}
```

## 修復符號連結處理順序問題

在測試中發現了單線程模式下符號連結處理的問題。原因是在單線程處理時，目錄項目的處理順序可能會導致符號連結在其指向的文件處理前就被處理，從而導致 inode 檢查失效。

解決方案是在處理前對目錄項目進行排序，確保符號連結在普通文件之後處理：

```rust
// Sort entries to ensure symlinks are processed after regular files
let mut sorted_entries = entries;  // 避免不必要的 clone
sorted_entries.sort_by(|a, b| {
  let a_is_symlink = a.is_symlink();
  let b_is_symlink = b.is_symlink();
  
  // 符號連結排在後面
  if a_is_symlink && !b_is_symlink {
    std::cmp::Ordering::Greater
  } else if !a_is_symlink && b_is_symlink {
    std::cmp::Ordering::Less
  } else {
    a.file_name().cmp(&b.file_name())
  }
});
```

## 添加掃描超時處理

為避免掃描大型目錄時卡住，添加了超時處理機制：

```rust
// 使用帶有超時的異步處理
let scan_task = tokio::time::timeout(
  std::time::Duration::from_secs(300), // 設置 5 分鐘超時
  tokio::task::spawn_blocking(move || {
    // Run the synchronous calculation using Rayon's parallel processing
    calculate_size_sync(
      &target_dir_clone,
      analytics_map_clone.clone(), 
      Some(&target_dir_clone),
      visited_inodes,
      processed_paths,
    )
  })
);

// 處理超時情況
match scan_task.await {
  Ok(task_result) => {
    if let Err(e) = task_result? {
      eprintln!("Error during directory calculation: {}", e);
      return Err(e);
    }
  },
  Err(_) => {
    eprintln!("Directory scan timed out after 5 minutes");
    return Err(std::io::Error::new(
      std::io::ErrorKind::TimedOut,
      "Directory scan timed out after 5 minutes"
    ));
  }
}
```

## 效能優化總結

1. 小型目錄（< 20 項）使用單線程處理，減少 Rayon 開銷
2. 大型目錄（>= 20 項）保持並行處理以提高效能
3. 通過排序確保符號連結在文件後處理，修復 inode 檢查
4. 統一並行和順序處理的行為，確保一致的結果
5. 避免不必要的 clone 操作，減少記憶體使用
6. 添加掃描超時處理，避免程序因掃描大型目錄而卡住
7. 優化了超時處理，確保用戶界面在超時時仍能收到通知

這些優化顯著提升了程序在各種目錄大小下的性能、穩定性和用戶體驗。
