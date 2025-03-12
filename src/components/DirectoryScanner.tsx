"use client";

import { useState, useEffect } from "react";
import { Button } from "@/components/ui/button";
import { open } from "@tauri-apps/plugin-dialog";
import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";
import { bytesToReadableSize, normalizePath, getParentPath } from "@/lib/utils";
import { TreeViewItem as BaseTreeViewItem } from "@/components/tree-view";
import {
  Folder,
  Percent,
  AlignJustify,
  SortAsc,
  SortDesc,
  ChevronRight,
  ChevronDown,
  Settings,
  HelpCircle,
  FolderOpen,
} from "lucide-react";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Input } from "@/components/ui/input";

// Extend the TreeViewItem interface to include size data
interface EnhancedTreeViewItem extends BaseTreeViewItem {
  sizeBytes: number;
  entryCount: number;
  allocatedBytes: number;
  fileCount: number;
  folderCount: number;
  percentOfParent: number;
  lastModified: string;
  owner: string;
  depth: number;
  backgroundColor?: string;
}

interface FileSystemEntry {
  path: string;
  size_bytes: number;
  entry_count: number;
  file_count: number;
  directory_count: number;
  allocated_bytes?: number;
  last_modified?: string;
  owner?: string;
}

export function DirectoryScanner() {
  const [selectedPath, setSelectedPath] = useState<string>("");
  const [entries, setEntries] = useState<FileSystemEntry[]>([]);
  const [displayedEntries, setDisplayedEntries] = useState<FileSystemEntry[]>(
    []
  );
  const [treeData, setTreeData] = useState<EnhancedTreeViewItem[]>([]);
  const [expandedItems, setExpandedItems] = useState<Set<string>>(new Set());
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sortOrder, setSortOrder] = useState<"asc" | "desc">("desc");
  const [sortBy, setSortBy] = useState<"size" | "count">("size");
  const [filterValue, setFilterValue] = useState("");
  const [currentTab, setCurrentTab] = useState<
    "file" | "home" | "scan" | "view" | "options" | "help"
  >("scan");
  const [totalSize, setTotalSize] = useState<number>(0);
  const [totalFiles, setTotalFiles] = useState<number>(0);
  const [freeSpace, setFreeSpace] = useState<string>("N/A");

  // Format size based on selected unit
  const formatSize = (sizeInBytes: number): string => {
    // Auto formatting
    return bytesToReadableSize(sizeInBytes);
  };

  // Generate color based on percentage
  const getColorForPercentage = (): string => {
    // Using a single color (yellow) with fixed opacity
    return "rgba(255, 215, 0, 0.4)"; // Gold color with fixed opacity
  };

  // Apply full-height style to ensure proper layout in all browsers
  useEffect(() => {
    const appRoot = document.getElementById('root');
    if (appRoot) {
      appRoot.style.height = '100vh';
      appRoot.style.display = 'flex';
      appRoot.style.flexDirection = 'column';
      appRoot.style.overflow = 'hidden';
    }
    return () => {
      if (appRoot) {
        appRoot.style.height = '';
        appRoot.style.display = '';
        appRoot.style.flexDirection = '';
        appRoot.style.overflow = '';
      }
    };
  }, []);

  // Handle window resize
  useEffect(() => {
    window.addEventListener("resize", handleResize);
    return () => window.removeEventListener("resize", handleResize);
  }, []);

  const handleResize = () => {
    // No specific resize handling needed for the new layout
  };

  // Handle filtering and sorting when entries change
  useEffect(() => {
    let filtered = entries;

    if (filterValue) {
      filtered = entries.filter((entry) =>
        normalizePath(entry.path).toLowerCase().includes(filterValue.toLowerCase())
      );
    }

    setDisplayedEntries(filtered);

    // Build tree structure
    buildTreeData(filtered);
  }, [entries, filterValue]);

  // Sort entries
  const sortEntries = (
    entriesToSort: FileSystemEntry[],
    order: "asc" | "desc",
    by: "size" | "count"
  ) => {
    return [...entriesToSort].sort((a, b) => {
      const valueA = by === "size" ? a.size_bytes : a.entry_count;
      const valueB = by === "size" ? b.size_bytes : b.entry_count;
      return order === "asc" ? valueA - valueB : valueB - valueA;
    });
  };

  const toggleSortOrder = () => {
    const newOrder = sortOrder === "asc" ? "desc" : "asc";
    setSortOrder(newOrder);
    setEntries(sortEntries(entries, newOrder, sortBy));
  };

  const changeSortBy = (by: "size" | "count") => {
    setSortBy(by);
    setEntries(sortEntries(entries, sortOrder, by));
  };

  const selectDirectory = async () => {
    try {
      // Open directory dialog
      const selected = await open({
        directory: true,
        multiple: false,
        title: "Select Directory to Scan",
      });

      if (selected && !Array.isArray(selected)) {
        setSelectedPath(selected);
        
        // Reset any previous scan data when selecting a new directory
        setEntries([]);
        setDisplayedEntries([]);
        setTreeData([]);
        setError(null);
      }
    } catch (err) {
      console.error("Error selecting directory:", err);
      setError("Failed to select directory");
    }
  };

  // Set up event listeners for scan progress events
  useEffect(() => {
    let unlistenEntry: Promise<UnlistenFn>;
    let unlistenComplete: Promise<UnlistenFn>;
    let bufferInterval: number | null = null;

    const setupListeners = async () => {
      // Create a buffer to collect entries before updating state
      let entriesBuffer: FileSystemEntry[] = [];

      const processBuffer = () => {
        if (entriesBuffer.length === 0) return;

        setEntries((prevEntries) => {
          // Create a map of the latest entries by path
          const entryMap = new Map<string, FileSystemEntry>();

          // First add all buffered entries to the map (duplicates will be overwritten)
          entriesBuffer.forEach((entry) => {
            entryMap.set(entry.path, entry);
          });

          // Create the updated entries array
          const updatedEntries = [...prevEntries];

          // Update or add entries from the buffer
          entryMap.forEach((newEntry) => {
            const existingEntryIndex = updatedEntries.findIndex(
              (entry) => entry.path === newEntry.path
            );

            if (existingEntryIndex >= 0) {
              // Update existing entry
              updatedEntries[existingEntryIndex] = newEntry;
            } else {
              // Add new entry
              updatedEntries.push(newEntry);
            }
          });

          // Sort entries
          const sortedEntries = sortEntries(updatedEntries, sortOrder, sortBy);

          // Clear the buffer
          entriesBuffer = [];

          return sortedEntries;
        });
      };

      const setupBufferProcessing = () => {
        if (bufferInterval === null) {
          bufferInterval = window.setInterval(processBuffer, 1000);
        }
      };

      // Clean up any existing listeners first (important for Windows)
      try {
        const cleanupListeners = async () => {
          try {
            // Try to clean up any previous listeners that might still be active
            const dummyFn = await listen("dummy-event", () => {});
            dummyFn();
          } catch {
            // Ignore errors
          }
        };

        await cleanupListeners();
      } catch {
        // Ignore cleanup errors
      }

      unlistenEntry = listen<FileSystemEntry>("directory-entry", (event) => {
        const newEntry = event.payload as FileSystemEntry;

        setEntries((prevEntries) => {
          // If we have fewer than 100 entries, update immediately
          if (prevEntries.length < 10) {
            const existingEntryIndex = prevEntries.findIndex(
              (entry) => entry.path === newEntry.path
            );

            const updatedEntries =
              existingEntryIndex >= 0
                ? [...prevEntries]
                : [...prevEntries, newEntry];

            // If we found an existing entry, update it
            if (existingEntryIndex >= 0) {
              updatedEntries[existingEntryIndex] = newEntry;
            }

            // Sort entries
            const sortedEntries = sortEntries(
              updatedEntries,
              sortOrder,
              sortBy
            );

            // Clear the buffer
            entriesBuffer = [];

            return sortedEntries;
          }

          // If we have 100+ entries, use the buffer approach
          entriesBuffer.push(newEntry);
          setupBufferProcessing();

          // If buffer reaches 10000 items, process it immediately
          if (entriesBuffer.length >= 10000) {
            processBuffer();
          }

          // Return unchanged state (buffer will update state later)
          return prevEntries;
        });
      });

      unlistenComplete = listen("scan-complete", () => {
        // Process any remaining entries in the buffer
        processBuffer();

        // Clear the interval if it exists
        if (bufferInterval !== null) {
          clearInterval(bufferInterval);
          bufferInterval = null;
        }

        setScanning(false);

        // Check free space
        if (selectedPath) {
          try {
            invoke("get_free_space", { path: selectedPath })
              .then((freeSpaceResult: unknown) => {
                setFreeSpace(bytesToReadableSize(freeSpaceResult as number));
              })
              .catch((err) => {
                console.error("Error getting free space:", err);
              });
          } catch (err) {
            console.error("Error getting free space:", err);
          }
        }
      });
    };

    if (selectedPath) {
      setupListeners();
    }

    return () => {
      // Clean up listeners when component unmounts
      if (unlistenEntry) {
        unlistenEntry.then((unlisten) => unlisten());
      }
      if (unlistenComplete) {
        unlistenComplete.then((unlisten) => unlisten());
      }
      // Clear the interval if it exists
      if (bufferInterval !== null) {
        clearInterval(bufferInterval);
        bufferInterval = null;
      }
    };
  }, [selectedPath, sortOrder, sortBy]);

  const startScan = async () => {
    if (!selectedPath) {
      setError("Please select a directory first");
      return;
    }

    try {
      // Clear state first
      setError(null);
      setEntries([]);
      setDisplayedEntries([]);
      setScanning(true);

      // Force a small delay to ensure any pending operations complete
      // This helps prevent issues on Windows
      await new Promise((resolve) => setTimeout(resolve, 50));

      // Invoke the Rust command to scan the directory
      await invoke("scan_directory_size", { path: selectedPath });
    } catch (err) {
      console.error("Error scanning directory:", err);
      setError(`Failed to scan directory: ${err}`);
      setScanning(false);
    }
  };

  // Smart expand the tree to show at least N items
  const smartExpandTree = (
    items: EnhancedTreeViewItem[],
    expandedSet: Set<string>,
    minItems = 10
  ): Set<string> => {
    let visibleCount = items.length;
    const newExpandedSet = new Set(expandedSet);

    // If we already have enough items visible, no need to expand further
    if (visibleCount >= minItems) {
      return newExpandedSet;
    }

    // Sort items by size to expand the largest directories first
    const sortedItems = [...items].sort((a, b) => b.sizeBytes - a.sizeBytes);

    // First pass: expand largest directories until we have enough visible items
    for (const item of sortedItems) {
      if (visibleCount >= minItems) break;

      if (item.children && item.children.length > 0) {
        newExpandedSet.add(item.id);
        visibleCount += item.children.length;
      }
    }

    // Second pass: if we still don't have enough items, recursively expand subdirectories
    if (visibleCount < minItems) {
      for (const item of sortedItems) {
        if (
          newExpandedSet.has(item.id) &&
          item.children &&
          item.children.length > 0
        ) {
          // Only recurse into directories we've already expanded
          const childItems = item.children as EnhancedTreeViewItem[];
          const updatedSet = smartExpandTree(
            childItems,
            newExpandedSet,
            minItems
          );

          // Update our expanded set with any new expansions
          for (const id of updatedSet) {
            newExpandedSet.add(id);
          }

          // Recalculate visible count
          visibleCount = countVisibleItems(items, newExpandedSet);

          if (visibleCount >= minItems) break;
        }
      }
    }

    return newExpandedSet;
  };

  // Count visible items in the tree
  const countVisibleItems = (
    items: EnhancedTreeViewItem[],
    expandedSet: Set<string>
  ): number => {
    let count = items.length;

    for (const item of items) {
      if (expandedSet.has(item.id) && item.children) {
        count += countVisibleItems(
          item.children as EnhancedTreeViewItem[],
          expandedSet
        );
      }
    }

    return count;
  };

  // Handle toggle expand
  const handleToggleExpand = (itemId: string) => {
    setExpandedItems((prev) => {
      const newSet = new Set(prev);
      if (newSet.has(itemId)) {
        newSet.delete(itemId);
      } else {
        newSet.add(itemId);
      }
      return newSet;
    });
  };

  // Build tree data from flat entries
  const buildTreeData = (entriesData: FileSystemEntry[]) => {
    if (entriesData.length === 0) {
      setTreeData([]);
      setTotalSize(0);
      setTotalFiles(0);
      return;
    }

    // Calculate total size and files
    const totalBytes = entriesData.reduce(
      (sum, entry) => sum + entry.size_bytes,
      0
    );
    const totalFileCount = entriesData.reduce(
      (sum, entry) => sum + entry.file_count,
      0
    );
    setTotalSize(totalBytes);
    setTotalFiles(totalFileCount);

    // Create a map to store nodes by path
    const pathMap = new Map<string, EnhancedTreeViewItem>();
    const rootItems: EnhancedTreeViewItem[] = [];

    // First pass: create all nodes and add them to the map
    entriesData.forEach((entry) => {
      // Normalize path for display
      const normalizedPath = normalizePath(entry.path);
      // Get just the filename or folder name
      const name = normalizePath(entry.path, true);
      const isDirectory = entry.directory_count > 0;
      const depth = normalizedPath.split('/').length - 1;

      // Calculate percentage of parent (will adjust later)
      const percentOfParent = 100;

      const item: EnhancedTreeViewItem = {
        id: entry.path, // Keep original path as ID
        name,
        type: isDirectory ? "directory" : "file",
        children: isDirectory ? [] : undefined,
        sizeBytes: entry.size_bytes,
        entryCount: entry.entry_count,
        allocatedBytes: entry.allocated_bytes || entry.size_bytes,
        fileCount: entry.file_count,
        folderCount: entry.directory_count,
        percentOfParent,
        lastModified: entry.last_modified || new Date().toLocaleDateString(),
        owner: entry.owner || "Unknown",
        depth,
        backgroundColor: getColorForPercentage(),
      };

      pathMap.set(entry.path, item);
    });

    // Second pass: build the tree structure and calculate percentages
    entriesData.forEach((entry) => {
      const item = pathMap.get(entry.path);
      if (!item) return;

      // Find the parent directory path
      const parentPath = getParentPath(entry.path);
      if (!parentPath) {
        // This is a root item
        rootItems.push(item);
        return;
      }

      const parent = pathMap.get(parentPath);

      if (parent && parent.children) {
        // Add to parent's children
        parent.children.push(item);

        // Update parent's file and folder counts
        if (item.type === "directory") {
          parent.folderCount += item.folderCount;
        } else {
          parent.fileCount += 1;
        }

        // Calculate percentage of parent
        item.percentOfParent =
          parent.sizeBytes > 0 ? (item.sizeBytes / parent.sizeBytes) * 100 : 0;

        // Update background color based on percentage
        item.backgroundColor = getColorForPercentage();
      } else {
        // No parent found, add to root
        rootItems.push(item);
      }
    });

    // Sort children by size
    const sortTreeItems = (items: EnhancedTreeViewItem[]) => {
      items.sort((a, b) => b.sizeBytes - a.sizeBytes);
      items.forEach((item) => {
        if (item.children && item.children.length > 0) {
          sortTreeItems(item.children as EnhancedTreeViewItem[]);
        }
      });
    };

    sortTreeItems(rootItems);

    setTreeData(rootItems);

    // Smart expand the tree to show at least 10 items
    setExpandedItems((prev) => smartExpandTree(rootItems, prev, 10));
  };

  return (
    <div className="h-full w-full flex flex-col overflow-hidden" style={{ height: '100vh', maxHeight: '100vh' }}>
      {/* Toolbar - Fixed at top */}
      <div className="border-b bg-muted/40 flex-shrink-0 z-20">
        <Tabs
          defaultValue={currentTab}
          onValueChange={(v: string) =>
            setCurrentTab(
              v as "file" | "home" | "scan" | "view" | "options" | "help"
            )
          }
        >
          <TabsList className="h-10">
            <TabsTrigger value="file" className="px-3 py-1">
              File
            </TabsTrigger>
            <TabsTrigger value="home" className="px-3 py-1">
              Home
            </TabsTrigger>
            <TabsTrigger value="scan" className="px-3 py-1">
              Scan
            </TabsTrigger>
            <TabsTrigger value="view" className="px-3 py-1">
              View
            </TabsTrigger>
            <TabsTrigger value="options" className="px-3 py-1">
              Options
            </TabsTrigger>
            <TabsTrigger value="help" className="px-3 py-1">
              Help
            </TabsTrigger>
          </TabsList>

          {/* Main Actions Toolbar */}
          <div className="flex items-center p-1 border-t">
            <TooltipProvider>
              <div className="flex border-r px-2">
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      variant="default"
                      size="sm"
                      className="h-14 w-14 flex flex-col items-center gap-1"
                    >
                      <Percent className="h-5 w-5" />
                      <span className="text-xs">Percent</span>
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>Show percentage of parent</TooltipContent>
                </Tooltip>
              </div>

              <div className="flex border-r px-2">
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      variant="ghost"
                      size="sm"
                      className="h-14 w-14 flex flex-col items-center gap-1"
                      onClick={selectDirectory}
                      disabled={scanning}
                    >
                      <Folder className="h-5 w-5" />
                      <span className="text-xs">Select</span>
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>Select a directory to scan</TooltipContent>
                </Tooltip>

                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      variant="ghost"
                      size="sm"
                      className="h-14 w-14 flex flex-col items-center gap-1"
                      onClick={startScan}
                      disabled={!selectedPath || scanning}
                    >
                      <AlignJustify className="h-5 w-5" />
                      <span className="text-xs">Scan</span>
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>Scan the selected directory</TooltipContent>
                </Tooltip>
              </div>

              <div className="flex border-r px-2">
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      variant="ghost"
                      size="sm"
                      className="h-14 w-14 flex flex-col items-center gap-1"
                      onClick={toggleSortOrder}
                    >
                      {sortOrder === "asc" ? (
                        <SortAsc className="h-5 w-5" />
                      ) : (
                        <SortDesc className="h-5 w-5" />
                      )}
                      <span className="text-xs">Sort</span>
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>Toggle sort order</TooltipContent>
                </Tooltip>

                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      variant="ghost"
                      size="sm"
                      className="h-14 w-14 flex flex-col items-center gap-1"
                    >
                      <Settings className="h-5 w-5" />
                      <span className="text-xs">Configure</span>
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>Configure display options</TooltipContent>
                </Tooltip>
              </div>

              <div className="flex px-2">
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      variant="ghost"
                      size="sm"
                      className="h-14 w-14 flex flex-col items-center gap-1"
                    >
                      <HelpCircle className="h-5 w-5" />
                      <span className="text-xs">Help</span>
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>View help information</TooltipContent>
                </Tooltip>
              </div>
            </TooltipProvider>
          </div>
        </Tabs>
      </div>

      {/* Path display - Fixed below toolbar */}
      {selectedPath && (
        <div className="p-2 text-sm bg-muted/20 border-b flex-shrink-0 z-10">{normalizePath(selectedPath)}</div>
      )}

      {/* Error display - Fixed below path display if there's an error */}
      {error && (
        <div className="p-2 text-sm text-destructive bg-destructive/10 flex-shrink-0 z-10">
          {error}
        </div>
      )}

      {/* Main content - Scrollable */}
      <div className="flex-grow flex flex-col overflow-hidden h-0">
        {treeData.length > 0 ? (
          <>
            <div className="p-2 flex justify-between items-center border-b flex-shrink-0 bg-background z-10">
              <Input
                type="text"
                placeholder="Filter files and folders..."
                value={filterValue}
                onChange={(e) => setFilterValue(e.target.value)}
                className="w-64 text-sm"
              />

              <div className="flex gap-2 items-center">
                <Button
                  onClick={toggleSortOrder}
                  variant="ghost"
                  size="sm"
                  className="text-xs"
                >
                  {sortOrder === "asc" ? "↑" : "↓"}
                </Button>

                <Button
                  onClick={() => changeSortBy("size")}
                  variant={sortBy === "size" ? "secondary" : "ghost"}
                  size="sm"
                  className="text-xs"
                >
                  Size
                </Button>

                <Button
                  onClick={() => changeSortBy("count")}
                  variant={sortBy === "count" ? "secondary" : "ghost"}
                  size="sm"
                  className="text-xs"
                >
                  Count
                </Button>
              </div>
            </div>

            <div className="flex-grow overflow-auto h-0">
              <TreeSizeView
                data={treeData}
                formatSize={formatSize}
                expandedItems={expandedItems}
                onToggleExpand={handleToggleExpand}
              />
            </div>
          </>
        ) : (
          <div className="flex items-center justify-center h-full">
            <div className="text-muted p-8 text-center">
              <FolderOpen className="w-12 h-12 mx-auto mb-2 opacity-20" />
              <h3 className="text-lg font-medium mb-1">
                No directory selected
              </h3>
              <p className="text-sm text-muted-foreground mb-4">
                Select a directory to scan and analyze its size
              </p>
              <Button onClick={selectDirectory}>Select Directory</Button>
            </div>
          </div>
        )}
      </div>

      {/* Status bar - Fixed at bottom */}
      <div className="flex justify-between border-t py-2 px-4 text-sm flex-shrink-0 bg-muted/20 z-10">
        <div>{freeSpace !== "N/A" && `Free space: ${freeSpace}`}</div>
        <div>
          {scanning && (
            <span className="text-muted-foreground animate-pulse">
              Scanning... {displayedEntries.length} entries found
            </span>
          )}
          {!scanning && treeData.length > 0 && (
            <span>
              {totalFiles.toLocaleString()} items, {formatSize(totalSize)}
            </span>
          )}
        </div>
      </div>
    </div>
  );
}

// Tree View component specifically designed for TreeSize
function TreeSizeView({
  data,
  formatSize,
  expandedItems,
  onToggleExpand,
}: {
  data: EnhancedTreeViewItem[];
  formatSize: (size: number) => string;
  expandedItems: Set<string>;
  onToggleExpand: (itemId: string) => void;
}) {
  return (
    <div className="h-full flex flex-col">
      {/* Header - Sticky */}
      <div className="grid grid-cols-[auto_1fr_repeat(6,auto)] sticky top-0 bg-muted/90 text-sm font-medium border-b select-none z-10 shadow-sm flex-shrink-0">
        <div className="w-6"></div>
        <div className="p-2">Name</div>
        <div className="p-2 text-right w-24">Size</div>
        <div className="p-2 text-right w-24">% of Parent</div>
        <div className="p-2 text-right w-16">Files</div>
        <div className="p-2 text-right w-16">Folders</div>
        <div className="p-2 w-32">Last Modified</div>
        <div className="p-2 w-32">Owner</div>
      </div>

      {/* Tree rows - Scrollable */}
      <div className="flex-grow overflow-auto h-0">
        {data.map((item) => (
          <TreeSizeItem
            key={item.id}
            item={item}
            formatSize={formatSize}
            expanded={expandedItems.has(item.id)}
            onToggleExpand={onToggleExpand}
            expandedItems={expandedItems}
          />
        ))}
      </div>
    </div>
  );
}

// Individual tree item component
function TreeSizeItem({
  item,
  formatSize,
  expanded,
  onToggleExpand,
  expandedItems,
}: {
  item: EnhancedTreeViewItem;
  formatSize: (size: number) => string;
  expanded: boolean;
  onToggleExpand: (itemId: string) => void;
  expandedItems: Set<string>;
}) {
  const toggleExpand = () => {
    if (item.children && item.children.length > 0) {
      onToggleExpand(item.id);
    }
  };

  const hasChildren = item.children && item.children.length > 0;
  const isSignificantSize = item.percentOfParent >= 20;

  return (
    <div>
      {/* Main row */}
      <div className="grid grid-cols-[auto_1fr_repeat(6,auto)] text-sm border-b hover:bg-muted/30 transition-colors">
        <div
          className="p-1 flex items-center justify-center cursor-pointer"
          onClick={toggleExpand}
        >
          {hasChildren &&
            (expanded ? (
              <ChevronDown className="h-4 w-4" />
            ) : (
              <ChevronRight className="h-4 w-4" />
            ))}
        </div>

        <div className="p-2 flex items-center gap-1 truncate relative">
          {/* Icon */}
          <div className="flex-shrink-0">
            {item.type === "directory" ? (
              <Folder className="h-4 w-4 text-blue-500" />
            ) : (
              <FileIcon className="h-4 w-4 text-gray-500" />
            )}
          </div>

          {/* Percentage background bar */}
          <div
            className="absolute left-7 top-0 bottom-0 bg-yellow-200 opacity-50 z-0"
            style={{
              width: `${Math.min(item.percentOfParent, 100)}%`,
              backgroundColor: item.backgroundColor || "rgba(255, 215, 0, 0.4)",
            }}
          />

          {/* File/folder name - bold if significant percentage */}
          <span
            className={`truncate z-10 relative ${
              isSignificantSize ? "font-bold" : "font-normal"
            }`}
          >
            {item.name}
          </span>
        </div>

        {/* Always show size */}
        <div className="p-2 text-right font-mono w-24">
          {formatSize(item.sizeBytes)}
        </div>

        {/* Always show percent of parent */}
        <div
          className={`p-2 text-right font-mono w-24 ${
            isSignificantSize ? "font-bold" : "font-normal"
          }`}
        >
          {item.percentOfParent.toFixed(1)}%
        </div>

        {/* Files count */}
        <div className="p-2 text-right font-mono w-16">
          {item.fileCount.toLocaleString()}
        </div>

        {/* Folders count */}
        <div className="p-2 text-right font-mono w-16">
          {item.folderCount.toLocaleString()}
        </div>

        <div className="p-2 w-32">{item.lastModified}</div>
        <div className="p-2 w-32">{item.owner}</div>
      </div>

      {/* Children */}
      {expanded && item.children && (
        <div className="pl-4">
          {(item.children as EnhancedTreeViewItem[]).map((child) => (
            <TreeSizeItem
              key={child.id}
              item={child}
              formatSize={formatSize}
              expanded={expandedItems.has(child.id)}
              onToggleExpand={onToggleExpand}
              expandedItems={expandedItems}
            />
          ))}
        </div>
      )}
    </div>
  );
}

// File icon component
function FileIcon(props: React.SVGProps<SVGSVGElement>) {
  return (
    <svg
      {...props}
      xmlns="http://www.w3.org/2000/svg"
      width="24"
      height="24"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d="M14.5 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7.5L14.5 2z" />
      <polyline points="14 2 14 8 20 8" />
    </svg>
  );
}
