"use client"

import { RefObject, useEffect, useMemo, useRef, useState } from "react"
import { invoke } from "@tauri-apps/api/core"
import { listen, UnlistenFn } from "@tauri-apps/api/event"
import { open } from "@tauri-apps/plugin-dialog"
import {
  AlignJustify,
  ChevronDown,
  ChevronRight,
  Folder,
  FolderOpen,
  HelpCircle,
  LoaderIcon,
  Percent,
  Settings,
} from "lucide-react"
import AutoSizer from "react-virtualized-auto-sizer"
import {
  FixedSizeList,
  FixedSizeList as List,
  ListChildComponentProps,
} from "react-window"

import { bytesToReadableSize, normalizePath } from "@/lib/utils"
import { Button } from "@/components/ui/button"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip"
import { TreeViewItem as BaseTreeViewItem } from "@/components/tree-view"

// Extend the TreeViewItem interface to include size data
interface EnhancedTreeViewItem extends BaseTreeViewItem {
  sizeBytes: number
  entryCount: number
  allocatedBytes: number
  fileCount: number
  folderCount: number
  percentOfParent: number
  lastModified: string
  owner: string
  depth: number
  backgroundColor?: string
  loading?: boolean // Add loading state for lazy-loaded directories
  loaded?: boolean // Track if directory contents have been loaded
}

interface FileSystemTreeNode {
  path: string
  name: string
  size_bytes: number
  entry_count: number
  file_count: number
  directory_count: number
  percent_of_parent: number
  children: FileSystemTreeNode[]
}

interface DirectoryScanResult {
  root_path: string
  tree: FileSystemTreeNode
  scan_time_ms: number
}

// New interface for flattened tree items
interface FlattenedTreeItem extends EnhancedTreeViewItem {
  isVisible: boolean
  nestingLevel: number
}

export function DirectoryScanner() {
  const [selectedPath, setSelectedPath] = useState<string>("")
  const [treeData, setTreeData] = useState<EnhancedTreeViewItem[]>([])
  const [expandedItems, setExpandedItems] = useState<Set<string>>(new Set())
  const [scanning, setScanning] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [currentTab, setCurrentTab] = useState<
    "file" | "home" | "scan" | "view" | "options" | "help"
  >("scan")
  const [totalSize, setTotalSize] = useState<number>(0)
  const [totalFiles, setTotalFiles] = useState<number>(0)
  const [freeSpace, setFreeSpace] = useState<string>("N/A")
  const [scanTimeMs, setScanTimeMs] = useState<number>(0)

  // Reference to the virtualized list for scrolling
  const listRef = useRef<List>(null)

  // Format size based on selected unit
  const formatSize = (sizeInBytes: number): string => {
    // Auto formatting
    return bytesToReadableSize(sizeInBytes)
  }

  // Generate color based on percentage
  const getColorForPercentage = (): string => {
    // Using a single color (yellow) with fixed opacity
    return "rgba(255, 215, 0, 0.4)" // Gold color with fixed opacity
  }

  // Apply full-height style to ensure proper layout in all browsers
  useEffect(() => {
    const appRoot = document.getElementById("root")
    if (appRoot) {
      appRoot.style.height = "100vh"
      appRoot.style.display = "flex"
      appRoot.style.flexDirection = "column"
      appRoot.style.overflow = "hidden"
    }
    return () => {
      if (appRoot) {
        appRoot.style.height = ""
        appRoot.style.display = ""
        appRoot.style.flexDirection = ""
        appRoot.style.overflow = ""
      }
    }
  }, [])

  // Handle window resize
  useEffect(() => {
    window.addEventListener("resize", handleResize)
    return () => window.removeEventListener("resize", handleResize)
  }, [])

  const handleResize = () => {
    // No specific resize handling needed for the new layout
  }

  // Handle expanding a directory - fetch children if needed
  const handleToggleExpand = async (itemId: string) => {
    // Find the item in the tree
    const findItem = (
      items: EnhancedTreeViewItem[]
    ): EnhancedTreeViewItem | null => {
      for (const item of items) {
        if (item.id === itemId) {
          return item
        }
        const children = item.children as EnhancedTreeViewItem[] | undefined
        if (children && children.length > 0) {
          const found = findItem(children)
          if (found) {
            return found
          }
        }
      }
      return null
    }

    // Toggle the expanded state
    setExpandedItems((prev) => {
      const newSet = new Set(prev)
      if (newSet.has(itemId)) {
        newSet.delete(itemId)
        return newSet
      } else {
        newSet.add(itemId)

        // Check if we need to load children
        const item = findItem(treeData)
        if (
          item &&
          item.type === "directory" &&
          (!item.loaded || (item.children && item.children.length === 0))
        ) {
          // Set loading state for this directory
          const updateLoadingState = (
            items: EnhancedTreeViewItem[],
            dirId: string,
            isLoading: boolean
          ): EnhancedTreeViewItem[] => {
            return items.map((item) => {
              if (item.id === dirId) {
                return {
                  ...item,
                  loading: isLoading,
                }
              } else if (item.children) {
                return {
                  ...item,
                  children: updateLoadingState(
                    item.children as EnhancedTreeViewItem[],
                    dirId,
                    isLoading
                  ),
                }
              }
              return item
            })
          }

          // Update loading state in the tree
          setTreeData((prevData) => updateLoadingState(prevData, itemId, true))

          // Load children asynchronously
          loadDirectoryChildren(itemId).catch((err) => {
            console.error("Error loading directory children:", err)
            setError(`Failed to load directory contents: ${err}`)

            // Update loading state in the tree (set to false)
            setTreeData((prevData) =>
              updateLoadingState(prevData, itemId, false)
            )
          })
        }

        return newSet
      }
    })
  }

  // Function to load directory children from Tauri
  const loadDirectoryChildren = async (directoryId: string) => {
    try {
      // Find the directory in the tree to update it
      const updateTreeWithChildren = (
        items: EnhancedTreeViewItem[],
        dirId: string,
        newChildren: EnhancedTreeViewItem[]
      ): EnhancedTreeViewItem[] => {
        return items.map((item) => {
          if (item.id === dirId) {
            return {
              ...item,
              children: newChildren,
              loaded: true,
              loading: false,
            }
          } else if (item.children) {
            const children = item.children as EnhancedTreeViewItem[]
            return {
              ...item,
              children: updateTreeWithChildren(children, dirId, newChildren),
            }
          }
          return item
        })
      }

      // Call Tauri to get directory children
      const result = await invoke<FileSystemTreeNode>(
        "get_directory_children",
        {
          path: directoryId,
        }
      )

      // Convert children to our tree format
      const children = result.children.map(
        (child) => convertTreeNodeToTreeViewItem(child, 1) // Set depth to 1
      )

      // Update tree data with new children
      setTreeData((prevData) =>
        updateTreeWithChildren(prevData, directoryId, children)
      )
    } catch (err) {
      console.error("Error loading directory children:", err)

      // Check if the error is related to missing cache
      if (String(err).includes("No scan data available")) {
        // If scan data is missing but we have a selected path, try to rescan
        if (selectedPath) {
          setError("Cache data unavailable. Rescanning directory...")

          // Reset loading state for the directory that failed
          const updateLoadingState = (
            items: EnhancedTreeViewItem[],
            dirId: string,
            isLoading: boolean
          ): EnhancedTreeViewItem[] => {
            return items.map((item) => {
              if (item.id === dirId) {
                return {
                  ...item,
                  loading: isLoading,
                }
              } else if (item.children) {
                return {
                  ...item,
                  children: updateLoadingState(
                    item.children as EnhancedTreeViewItem[],
                    dirId,
                    isLoading
                  ),
                }
              }
              return item
            })
          }

          // Update loading state in the tree (set to false)
          setTreeData((prevData) =>
            updateLoadingState(prevData, directoryId, false)
          )

          // Attempt to rescan
          try {
            await startScan()
          } catch (scanErr) {
            setError(`Failed to rescan: ${scanErr}`)
          }
        }
      }

      throw err
    }
  }

  // Convert the Rust tree to our enhanced tree view format
  const convertTreeNodeToTreeViewItem = (
    node: FileSystemTreeNode,
    depth: number = 0
  ): EnhancedTreeViewItem => {
    return {
      id: node.path,
      name: node.name,
      type: node.directory_count > 0 ? "directory" : "file",
      sizeBytes: node.size_bytes,
      entryCount: node.entry_count,
      allocatedBytes: node.size_bytes, // Use size_bytes as allocatedBytes
      fileCount: node.file_count,
      folderCount: node.directory_count,
      percentOfParent: node.percent_of_parent,
      lastModified: new Date().toLocaleDateString(), // Default date
      owner: "Unknown",
      depth,
      backgroundColor: getColorForPercentage(),
      children: node.children.map((child) =>
        convertTreeNodeToTreeViewItem(child, depth + 1)
      ),
      loaded: node.children.length > 0, // Track if children are already loaded
    }
  }

  const selectDirectory = async () => {
    try {
      // Open directory dialog
      const selected = await open({
        directory: true,
        multiple: false,
        title: "Select Directory to Scan",
      })

      if (selected && !Array.isArray(selected)) {
        // If we're changing directories, clear the cache first
        if (selectedPath && selectedPath !== selected) {
          try {
            await invoke("clear_scan_cache")
          } catch (err) {
            console.error("Failed to clear scan cache:", err)
            // Continue anyway
          }
        }

        setSelectedPath(selected)

        // Reset any previous scan data when selecting a new directory
        setTreeData([])
        setError(null)
      }
    } catch (err) {
      console.error("Error selecting directory:", err)
      setError("Failed to select directory")
    }
  }

  // Set up event listener for scan result event
  useEffect(() => {
    let unlistenResult: Promise<UnlistenFn>
    let unlistenComplete: Promise<UnlistenFn>

    const setupListeners = async () => {
      // Clean up any existing listeners first
      try {
        const cleanupListeners = async () => {
          try {
            const dummyFn = await listen("dummy-event", () => {})
            dummyFn()
          } catch {
            // Ignore errors
          }
        }

        await cleanupListeners()
      } catch {
        // Ignore cleanup errors
      }

      // Listen for the complete scan result
      unlistenResult = listen<DirectoryScanResult>("scan-result", (event) => {
        const result = event.payload as DirectoryScanResult

        console.log("Received scan result:", result)

        // Set scan time
        setScanTimeMs(result.scan_time_ms)

        // Calculate total size and files
        if (result.tree) {
          setTotalSize(result.tree.size_bytes)
          setTotalFiles(result.tree.file_count)

          // Convert tree to our format
          const convertedTree = [convertTreeNodeToTreeViewItem(result.tree)]
          setTreeData(convertedTree)

          // No smart expand - only expand the root by default
          const newExpandedSet = new Set<string>([result.tree.path])
          setExpandedItems(newExpandedSet)
        }

        // Set scanning to false
        setScanning(false)
      })

      // Also listen for scan-complete for backward compatibility
      unlistenComplete = listen("scan-complete", () => {
        setScanning(false)

        // Check free space
        if (selectedPath) {
          try {
            invoke("get_free_space", { path: selectedPath })
              .then((freeSpaceResult: unknown) => {
                setFreeSpace(bytesToReadableSize(freeSpaceResult as number))
              })
              .catch((err) => {
                console.error("Error getting free space:", err)
              })
          } catch (err) {
            console.error("Error getting free space:", err)
          }
        }
      })
    }

    if (selectedPath) {
      setupListeners()
    }

    return () => {
      // Clean up listeners when component unmounts
      if (unlistenResult) {
        unlistenResult.then((unlisten) => unlisten())
      }
      if (unlistenComplete) {
        unlistenComplete.then((unlisten) => unlisten())
      }
    }
  }, [selectedPath])

  const startScan = async () => {
    if (!selectedPath) {
      setError("Please select a directory first")
      return
    }

    try {
      // Clear state first
      setError(null)
      setTreeData([])
      setScanTimeMs(0)
      setScanning(true)

      // Force a small delay to ensure any pending operations complete
      await new Promise((resolve) => setTimeout(resolve, 50))

      // Invoke the Rust command to scan the directory
      await invoke("scan_directory_size", { path: selectedPath })
    } catch (err) {
      console.error("Error scanning directory:", err)
      setError(`Failed to scan directory: ${err}`)
      setScanning(false)
    }
  }

  // Add a cleanup effect to clear the cache when component unmounts
  useEffect(() => {
    return () => {
      // Attempt to clear the cache when the component unmounts
      invoke("clear_scan_cache").catch((err) => {
        console.error("Failed to clear scan cache on unmount:", err)
      })
    }
  }, [])

  return (
    <div
      className="h-full w-full flex flex-col overflow-hidden"
      style={{ height: "100vh", maxHeight: "100vh" }}
    >
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
        <div className="p-2 text-sm bg-muted/20 border-b flex-shrink-0 z-10">
          {normalizePath(selectedPath)}
        </div>
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
          <div className="flex-grow overflow-hidden h-0">
            <TreeSizeView
              data={treeData}
              formatSize={formatSize}
              expandedItems={expandedItems}
              onToggleExpand={handleToggleExpand}
              listRef={listRef as RefObject<FixedSizeList>}
            />
          </div>
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
              Scanning... Please wait
            </span>
          )}
          {!scanning && treeData.length > 0 && (
            <span>
              {totalFiles.toLocaleString()} items, {formatSize(totalSize)}
              {scanTimeMs > 0 &&
                ` (scanned in ${(scanTimeMs / 1000).toFixed(2)}s)`}
            </span>
          )}
        </div>
      </div>
    </div>
  )
}

// Flatten the tree for virtualization
function flattenTree(
  items: EnhancedTreeViewItem[],
  expandedItems: Set<string>,
  nestingLevel = 0
): FlattenedTreeItem[] {
  let result: FlattenedTreeItem[] = []

  for (const item of items) {
    // Add the current item
    result.push({
      ...item,
      isVisible: true,
      nestingLevel,
    })

    // If this item is expanded and has children, add them too
    if (
      expandedItems.has(item.id) &&
      item.children &&
      item.children.length > 0
    ) {
      result = result.concat(
        flattenTree(
          item.children as EnhancedTreeViewItem[],
          expandedItems,
          nestingLevel + 1
        )
      )
    }
  }

  return result
}

// Tree View component specifically designed for TreeSize
function TreeSizeView({
  data,
  formatSize,
  expandedItems,
  onToggleExpand,
  listRef,
}: {
  data: EnhancedTreeViewItem[]
  formatSize: (size: number) => string
  expandedItems: Set<string>
  onToggleExpand: (itemId: string) => void
  listRef?: React.RefObject<List>
}) {
  // Flatten the tree for virtualization
  const flattenedItems = useMemo(
    () => flattenTree(data, expandedItems),
    [data, expandedItems]
  )

  // Row height for virtualization
  const ROW_HEIGHT = 36 // Adjust based on your actual row height

  // Render a row in the virtualized list
  const Row = ({ index, style }: ListChildComponentProps) => {
    const item = flattenedItems[index]
    if (!item) return null

    const isExpanded = expandedItems.has(item.id)
    const isLoading = item.loading || false
    const isSignificantSize = item.percentOfParent >= 20

    return (
      <div
        style={style}
        className="border-b hover:bg-muted/30 transition-colors"
      >
        <div className="grid grid-cols-[auto_1fr_repeat(6,auto)] text-sm h-full">
          {/* Expand/collapse button with proper indentation */}
          <div
            className="p-1 flex items-center justify-center cursor-pointer"
            style={{ paddingLeft: `${item.nestingLevel * 16}px` }}
            onClick={() => item.type === "directory" && onToggleExpand(item.id)}
          >
            {item.type === "directory" &&
              (isLoading ? (
                <LoaderIcon className="h-4 w-4 animate-spin" />
              ) : isExpanded ? (
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
                backgroundColor:
                  item.backgroundColor || "rgba(255, 215, 0, 0.4)",
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
      </div>
    )
  }

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

      {/* Virtualized tree rows */}
      <div className="flex-grow h-0">
        <AutoSizer>
          {({ height, width }) => (
            <List
              ref={listRef}
              height={height}
              width={width}
              itemCount={flattenedItems.length}
              itemSize={ROW_HEIGHT}
            >
              {Row}
            </List>
          )}
        </AutoSizer>
      </div>
    </div>
  )
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
  )
}
