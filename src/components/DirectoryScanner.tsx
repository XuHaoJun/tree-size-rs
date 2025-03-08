"use client";

import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { open } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";
import { bytesToReadableSize } from "@/lib/utils";

interface FileSystemEntry {
  path: string;
  size_bytes: number;
}

interface EventPayload<T> {
  payload: T;
}

export function DirectoryScanner() {
  const [selectedPath, setSelectedPath] = useState<string>("");
  const [entries, setEntries] = useState<FileSystemEntry[]>([]);
  const [displayedEntries, setDisplayedEntries] = useState<FileSystemEntry[]>([]);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sortOrder, setSortOrder] = useState<"asc" | "desc">("desc");
  const [filterValue, setFilterValue] = useState("");

  useEffect(() => {
    // Set up listeners for directory scan events
    let unlistenEntry: Promise<UnlistenFn>;
    let unlistenComplete: Promise<UnlistenFn>;

    const setupListeners = async () => {
      unlistenEntry = listen<FileSystemEntry>("directory-entry", (event: EventPayload<FileSystemEntry>) => {
        setEntries((prevEntries) => {
          const newEntry = event.payload;
          const existingEntryIndex = prevEntries.findIndex(entry => entry.path === newEntry.path);
          
          let updatedEntries;
          if (existingEntryIndex >= 0) {
            // Update existing entry
            updatedEntries = [...prevEntries];
            updatedEntries[existingEntryIndex] = newEntry;
          } else {
            // Add new entry
            updatedEntries = [...prevEntries, newEntry];
          }
          
          // Sort entries
          return sortEntries(updatedEntries, sortOrder);
        });
      });

      unlistenComplete = listen("scan-complete", () => {
        setScanning(false);
      });
    };

    setupListeners();

    return () => {
      // Clean up listeners when component unmounts
      if (unlistenEntry) {
        unlistenEntry.then(unlisten => unlisten());
      }
      if (unlistenComplete) {
        unlistenComplete.then(unlisten => unlisten());
      }
    };
  }, [sortOrder]);

  // Update displayed entries when entries or filter changes
  useEffect(() => {
    let filteredEntries = entries;
    
    if (filterValue) {
      filteredEntries = entries.filter(entry => 
        entry.path.toLowerCase().includes(filterValue.toLowerCase())
      );
    }
    
    setDisplayedEntries(filteredEntries);
  }, [entries, filterValue]);

  const sortEntries = (entriesToSort: FileSystemEntry[], order: "asc" | "desc") => {
    return [...entriesToSort].sort((a, b) => {
      return order === "asc" 
        ? a.size_bytes - b.size_bytes 
        : b.size_bytes - a.size_bytes;
    });
  };

  const toggleSortOrder = () => {
    const newOrder = sortOrder === "asc" ? "desc" : "asc";
    setSortOrder(newOrder);
    setEntries(sortEntries(entries, newOrder));
  };

  const selectDirectory = async () => {
    try {
      // Open directory dialog
      const selected = await open({
        directory: true,
        multiple: false,
        title: "Select Directory to Scan"
      });

      if (selected && !Array.isArray(selected)) {
        setSelectedPath(selected);
      }
    } catch (err) {
      console.error("Error selecting directory:", err);
      setError("Failed to select directory");
    }
  };

  const startScan = async () => {
    if (!selectedPath) {
      setError("Please select a directory first");
      return;
    }

    try {
      setEntries([]);
      setDisplayedEntries([]);
      setScanning(true);
      setError(null);

      // Invoke the Rust command to scan the directory
      await invoke("scan_directory_size", { path: selectedPath });
    } catch (err) {
      console.error("Error scanning directory:", err);
      setError(`Failed to scan directory: ${err}`);
      setScanning(false);
    }
  };

  return (
    <div className="w-full max-w-4xl mx-auto p-4">
      <h1 className="text-2xl font-bold mb-4">Directory Size Scanner</h1>
      
      <div className="flex flex-col gap-4 mb-6">
        <div className="flex gap-2 items-center">
          <Button onClick={selectDirectory} disabled={scanning}>
            Select Directory
          </Button>
          <span className="truncate max-w-md">
            {selectedPath || "No directory selected"}
          </span>
        </div>
        
        <Button 
          onClick={startScan} 
          disabled={!selectedPath || scanning}
          className="w-fit"
        >
          {scanning ? "Scanning..." : "Start Scan"}
        </Button>
        
        {error && (
          <div className="text-red-500 text-sm">{error}</div>
        )}
      </div>

      {scanning && (
        <div className="mb-4 text-blue-500">
          Scanning in progress... Real-time results will appear below.
        </div>
      )}

      {entries.length > 0 && (
        <div className="mb-4 flex items-center gap-4">
          <div className="flex-1">
            <input
              type="text"
              placeholder="Filter by path..."
              className="w-full p-2 border rounded"
              value={filterValue}
              onChange={(e) => setFilterValue(e.target.value)}
            />
          </div>
          
          <Button onClick={toggleSortOrder} variant="outline" size="sm">
            Size {sortOrder === "asc" ? "↑" : "↓"}
          </Button>
        </div>
      )}

      <div className="border rounded-md">
        <div className="grid grid-cols-[1fr_auto] font-medium p-3 border-b bg-gray-50 dark:bg-gray-800">
          <div>Path</div>
          <div>Size</div>
        </div>
        
        <div className="max-h-[calc(100vh-360px)] overflow-y-auto">
          {displayedEntries.length > 0 ? (
            displayedEntries.map((entry, index) => (
              <div 
                key={index} 
                className="grid grid-cols-[1fr_auto] p-3 text-sm border-b last:border-0 hover:bg-gray-50 dark:hover:bg-gray-800"
              >
                <div className="truncate">{entry.path}</div>
                <div className="text-right font-mono">
                  {bytesToReadableSize(entry.size_bytes)}
                </div>
              </div>
            ))
          ) : (
            <div className="p-3 text-center text-gray-500">
              {scanning ? "Waiting for results..." : "No entries to display"}
            </div>
          )}
        </div>
      </div>
      
      {entries.length > 0 && (
        <div className="mt-4 text-sm text-gray-500">
          Found {entries.length} entries {filterValue && `(${displayedEntries.length} shown)`}
        </div>
      )}
    </div>
  );
} 