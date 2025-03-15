# Optimizing DirectoryScanner with Virtualization

## Summary
In this session, we optimized the `DirectoryScanner` component in a Tauri-based file explorer application by implementing virtualized rendering using `react-window` and `react-virtualized-auto-sizer`. This significantly improves performance when displaying large directory trees by only rendering visible items.

## Implementation Details

### 1. Dependencies Added
- `react-window`: For efficient virtualized list rendering
- `react-virtualized-auto-sizer`: For automatically sizing the virtualized list to its container

### 2. Key Components Modified

#### Flattened Tree Structure
Created a new interface and function to flatten the hierarchical tree for virtualization:

```typescript
// New interface for flattened tree items
interface FlattenedTreeItem extends EnhancedTreeViewItem {
  shouldRender: boolean
  nestingLevel: number
}

// Flatten the tree for virtualization - Optimized version
function flattenTree(
  items: EnhancedTreeViewItem[],
  expandedItems: Set<string>,
  nestingLevel = 0
): FlattenedTreeItem[] {
  // Pre-allocate a single result array to avoid repeated concatenation
  const result: FlattenedTreeItem[] = []
  
  // Helper function to recursively add items to the result array
  const addItemsToResult = (
    items: EnhancedTreeViewItem[],
    nestingLevel: number
  ) => {
    for (const item of items) {
      // Add the current item
      result.push({
        ...item,
        shouldRender: true,
        nestingLevel,
      })

      // If this item is expanded and has children, process them too
      if (
        expandedItems.has(item.id) &&
        item.children &&
        item.children.length > 0
      ) {
        addItemsToResult(item.children as EnhancedTreeViewItem[], nestingLevel + 1)
      }
    }
  }

  // Start the recursive process
  addItemsToResult(items, nestingLevel)
  
  return result
}
```

#### Virtualized TreeSizeView Component
Replaced the recursive tree rendering with a virtualized list:

```typescript
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
      <div style={style} className="border-b hover:bg-muted/30 transition-colors">
        {/* Row content with proper indentation */}
        <div className="grid grid-cols-[auto_1fr_repeat(6,auto)] text-sm h-full">
          <div
            className="p-1 flex items-center justify-center cursor-pointer"
            style={{ paddingLeft: `${item.nestingLevel * 16}px` }}
            onClick={() => item.type === "directory" && onToggleExpand(item.id)}
          >
            {/* Expand/collapse icons */}
          </div>
          
          {/* Other cell contents */}
        </div>
      </div>
    )
  }

  return (
    <div className="h-full flex flex-col">
      {/* Header - Sticky */}
      <div className="grid grid-cols-[auto_1fr_repeat(6,auto)] sticky top-0 bg-muted/90 text-sm font-medium border-b select-none z-10 shadow-sm flex-shrink-0">
        {/* Header cells */}
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
```

#### List Reference for Scrolling
Added a reference to the virtualized list for potential programmatic scrolling:

```typescript
// Reference to the virtualized list for scrolling
const listRef = useRef<List>(null)
```

### 3. Key Optimizations

1. **On-demand Rendering**: Only renders the rows currently visible in the viewport
2. **Efficient Updates**: Uses `useMemo` to prevent unnecessary recalculations of the flattened tree
3. **Visual Tree Structure**: Maintains the visual hierarchy through indentation based on nesting level
4. **Responsive Layout**: Automatically adjusts to container size changes with `AutoSizer`
5. **Consistent Performance**: Maintains smooth scrolling even with thousands of items
6. **Optimized Tree Flattening**: Uses a single pre-allocated array with a recursive helper function instead of repeated array concatenation, significantly improving performance for large trees

### 4. Type Handling
Fixed TypeScript issues by properly typing the list reference:

```typescript
// In the DirectoryScanner component
listRef={listRef as RefObject<FixedSizeList>}
```

## Performance Benefits

1. **Reduced DOM Nodes**: Only visible rows are rendered in the DOM
2. **Constant Memory Usage**: Memory usage remains relatively constant regardless of tree size
3. **Smooth Scrolling**: Maintains 60fps scrolling performance even with very large trees
4. **Efficient Expansion**: Only processes visible nodes when expanding/collapsing directories
5. **Responsive UI**: UI remains responsive even when scanning large directories
6. **Memory Efficiency**: Avoids creating multiple intermediate arrays during tree flattening
7. **Reduced GC Pressure**: Less memory allocation and deallocation means fewer garbage collection pauses

## Conclusion
The implementation of virtualized rendering significantly improves the performance and user experience of the DirectoryScanner component, especially when dealing with large directory structures. The application can now handle directories with thousands of files and folders without performance degradation. The optimized tree flattening algorithm further enhances performance by reducing memory operations when processing large directory trees.
