Example to Illustrate
Case 1: Small Directory (Leaf Node in $INDEX_ROOT)
Directory with 2 files: “file1.txt” and “file2.txt”.

Index Node Header:
- entries_offset: 16
- total_size: 64    // Size of entries in this node
- allocated_size: 128
- flags: 0x00      // Leaf node, no subnodes

Index Entries:
- Entry 1: [file_reference: 100, length: 32, name: "file1.txt"]
- Entry 2: [file_reference: 101, length: 32, name: "file2.txt", flags: 0x02]
total_size = 32 + 32 = 64 bytes: Only the two entries in this node. No subnodes, so this is the full directory.
Case 2: Larger Directory (Branch Node in $INDEX_ROOT + Subnode in $INDEX_ALLOCATION)
Directory with many files, split across nodes.

$INDEX_ROOT:
Index Node Header:
- entries_offset: 16
- total_size: 40    // Size of branch entry
- allocated_size: 128
- flags: 0x01      // Has subnodes

Index Entry:
- [file_reference: ignored, length: 40, name: "file_m", flags: 0x01, subnode_vcn: 1000]

$INDEX_ALLOCATION (Block at VCN 1000):
Index Node Header:
- entries_offset: 24
- total_size: 96    // Size of entries in this block
- allocated_size: 4096
- flags: 0x00      // Leaf node

Index Entries:
- Entry 1: [file_reference: 200, length: 32, name: "file_m1.txt"]
- Entry 2: [file_reference: 201, length: 32, name: "file_m2.txt"]
- Entry 3: [file_reference: 202, length: 32, name: "file_m3.txt", flags: 0x02]
$INDEX_ROOT’s total_size = 40: Size of the single branch entry pointing to the subnode.
$INDEX_ALLOCATION’s total_size = 96: Size of the three leaf entries in that block (32 + 32 + 32).
Total descendants: Not reflected in either total_size. You’d need to traverse subnodes to get the full count or size.
Implications for Your du-Like Tool
For calculating the total size of a directory (e.g., C:\Users\John\Downloads):

Immediate Children: total_size in $INDEX_ROOT or an $INDEX_ALLOCATION block tells you the size of the index entries in that node, but this isn’t the file sizes—it’s just metadata overhead.
All Descendants: To get the total size of all files and subdirs (including all depths), you must:
Parse $INDEX_ROOT to get initial file_reference numbers.
If flags & 0x01, follow subnode_vcn to $INDEX_ALLOCATION blocks.
Recursively process each referenced MFT record to sum their $DATA sizes (for files) or repeat the process (for subdirs).
The total_size field helps you iterate entries within a node (stopping when you’ve processed total_size bytes), but it doesn’t give you the recursive size of the directory’s contents—that requires traversing the B-tree.

Conclusion
total_size: Represents the size of index entries in the current node only (immediate children’s metadata), not all descendants.
Not Recursive: It doesn’t include subnodes in $INDEX_ALLOCATION or deeper levels of the B-tree.
Your Tool: Use total_size to parse entries in each node, but calculate the actual directory size by summing $DATA attributes of all referenced files and subdirs recursively.
Does this answer your question fully? If you’d like, I can show how this fits into your Rust code’s get_children method with more detail!