import { clsx, type ClassValue } from "clsx"
import { twMerge } from "tailwind-merge"

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

/**
 * Converts bytes to a human-readable size format
 * @param bytes The number of bytes
 * @returns Formatted string (e.g., "1.5 MB")
 */
export function bytesToReadableSize(bytes: number): string {
  if (bytes === 0) return '0 Bytes';
  
  const sizes = ['Bytes', 'KB', 'MB', 'GB', 'TB', 'PB'];
  const i = Math.floor(Math.log(bytes) / Math.log(1024));
  
  return parseFloat((bytes / Math.pow(1024, i)).toFixed(2)) + ' ' + sizes[i];
}

/**
 * Normalizes a path for display, handling Windows-specific issues
 * - Removes \\?\ prefix from Windows long paths
 * - Normalizes backslashes to forward slashes for consistent display
 * - Returns just the filename for the last part in a path
 * 
 * @param path The path to normalize
 * @param getLastPartOnly If true, returns only the last part of the path
 * @returns Normalized path
 */
export function normalizePath(path: string, getLastPartOnly: boolean = false): string {
  // Remove the Windows long path prefix if present
  let normalizedPath = path.replace(/^\\\\\?\\/, '');
  
  // Replace backslashes with forward slashes for consistent display
  normalizedPath = normalizedPath.replace(/\\/g, '/');
  
  // Return just the last part if requested
  if (getLastPartOnly) {
    const parts = normalizedPath.split('/');
    return parts[parts.length - 1] || normalizedPath;
  }
  
  return normalizedPath;
}

/**
 * Get the parent path from a path string
 * @param path The path to get the parent of
 * @returns Parent path
 */
export function getParentPath(path: string): string {
  const normalizedPath = normalizePath(path);
  const lastSlashIndex = normalizedPath.lastIndexOf('/');
  if (lastSlashIndex === -1) {
    return '';
  }
  return normalizedPath.substring(0, lastSlashIndex);
}
