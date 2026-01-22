#!/usr/bin/env python3
"""
Automatically replace heap-allocating format! macros with safe_print!

This script finds patterns like:
    console::print(&format!(...))
    console::print(&alloc::format!(...))
    crate::console::print(&alloc::format!(...))

And replaces them with:
    safe_print!(SIZE, ...)
    crate::safe_print!(SIZE, ...)

The SIZE is estimated based on the format string length + some padding for arguments.
"""

import re
import sys
import os
from pathlib import Path

# Estimate buffer size based on format string and argument count
def estimate_size(format_str: str, arg_count: int) -> int:
    """Estimate buffer size needed for the formatted output."""
    # Start with format string length (minus format specifiers)
    base_len = len(format_str)
    
    # Add padding for each argument (hex numbers can be up to 18 chars, decimals up to 20)
    arg_padding = arg_count * 20
    
    # Round up to next power of 2 or nice number
    total = base_len + arg_padding
    
    # Use standard sizes
    if total <= 32:
        return 32
    elif total <= 64:
        return 64
    elif total <= 96:
        return 96
    elif total <= 128:
        return 128
    elif total <= 160:
        return 160
    elif total <= 192:
        return 192
    elif total <= 256:
        return 256
    elif total <= 384:
        return 384
    elif total <= 512:
        return 512
    else:
        return 1024


def count_format_args(format_str: str) -> int:
    """Count the number of format arguments in a format string."""
    # Count {} placeholders (including {:x}, {:?}, etc.)
    return len(re.findall(r'\{[^}]*\}', format_str))


def find_matching_paren(text: str, start: int) -> int:
    """Find the matching closing parenthesis for the one at start position."""
    depth = 0
    i = start
    while i < len(text):
        c = text[i]
        if c == '(':
            depth += 1
        elif c == ')':
            depth -= 1
            if depth == 0:
                return i
        elif c == '"':
            # Skip string literals
            i += 1
            while i < len(text) and text[i] != '"':
                if text[i] == '\\':
                    i += 1  # Skip escaped char
                i += 1
        i += 1
    return -1


def extract_format_args(text: str) -> tuple[str, str, int]:
    """
    Extract format string and arguments from format!(...) call.
    Returns (format_string, all_args_str, arg_count)
    """
    # Find the opening paren of format!
    match = re.search(r'format!\s*\(', text)
    if not match:
        return None, None, 0
    
    start = match.end() - 1  # Position of opening paren
    end = find_matching_paren(text, start)
    if end == -1:
        return None, None, 0
    
    inner = text[start + 1:end].strip()
    
    # Extract format string (first argument, a string literal)
    if not inner.startswith('"'):
        return None, None, 0
    
    # Find end of format string
    i = 1
    while i < len(inner):
        if inner[i] == '"' and inner[i-1] != '\\':
            break
        i += 1
    
    if i >= len(inner):
        return None, None, 0
    
    format_str = inner[1:i]  # Content inside quotes
    
    # Get the rest (arguments)
    rest = inner[i + 1:].strip()
    if rest.startswith(','):
        rest = rest[1:].strip()
    
    # Count arguments
    arg_count = count_format_args(format_str)
    
    return format_str, rest, arg_count


def process_file(filepath: Path, dry_run: bool = True) -> int:
    """Process a single file, replacing format! with safe_print!."""
    
    try:
        content = filepath.read_text()
    except Exception as e:
        print(f"Error reading {filepath}: {e}")
        return 0
    
    original_content = content
    replacements = 0
    
    # Patterns to match (order matters - more specific first)
    patterns = [
        # crate::console::print(&alloc::format!(...))
        (r'crate::console::print\s*\(\s*&\s*alloc::format!\s*\(', 'crate::safe_print!('),
        # console::print(&alloc::format!(...))
        (r'console::print\s*\(\s*&\s*alloc::format!\s*\(', 'crate::safe_print!('),
        # crate::console::print(&format!(...))
        (r'crate::console::print\s*\(\s*&\s*format!\s*\(', 'crate::safe_print!('),
        # console::print(&format!(...))
        (r'console::print\s*\(\s*&\s*format!\s*\(', 'crate::safe_print!('),
    ]
    
    for pattern, replacement_prefix in patterns:
        # Find all matches
        while True:
            match = re.search(pattern, content)
            if not match:
                break
            
            # Find the full extent of the format! call
            format_start = match.start()
            
            # Find "format!(" within the match
            format_match = re.search(r'format!\s*\(', content[format_start:])
            if not format_match:
                break
            
            paren_start = format_start + format_match.end() - 1
            paren_end = find_matching_paren(content, paren_start)
            
            if paren_end == -1:
                print(f"Warning: Could not find matching paren in {filepath}")
                break
            
            # Find the closing ));
            # After format!(...) we need to find the closing ) of console::print
            rest = content[paren_end + 1:].lstrip()
            if not rest.startswith(')'):
                # Try to find it
                close_idx = content.find(')', paren_end + 1)
                if close_idx == -1:
                    break
                full_end = close_idx + 1
            else:
                full_end = paren_end + 1 + len(content[paren_end + 1:]) - len(rest) + 1
            
            # Extract format string and args
            format_call = content[paren_start:paren_end + 1]
            format_str, args, arg_count = extract_format_args('format!' + format_call)
            
            if format_str is None:
                # Fallback: just use a large buffer
                size = 256
            else:
                size = estimate_size(format_str, arg_count)
            
            # Build replacement
            inner_content = content[paren_start + 1:paren_end]
            new_call = f'{replacement_prefix}{size}, {inner_content})'
            
            # Replace
            content = content[:format_start] + new_call + content[full_end:]
            replacements += 1
    
    if replacements > 0:
        if dry_run:
            print(f"Would fix {replacements} occurrences in {filepath}")
        else:
            filepath.write_text(content)
            print(f"Fixed {replacements} occurrences in {filepath}")
    
    return replacements


def main():
    import argparse
    
    parser = argparse.ArgumentParser(description='Replace format! macros with safe_print!')
    parser.add_argument('--dry-run', action='store_true', help='Show what would be changed without modifying files')
    parser.add_argument('--file', type=str, help='Process a single file')
    parser.add_argument('--src', type=str, default='src', help='Source directory to process')
    args = parser.parse_args()
    
    if args.file:
        files = [Path(args.file)]
    else:
        src_dir = Path(args.src)
        if not src_dir.exists():
            print(f"Source directory {src_dir} does not exist")
            sys.exit(1)
        files = list(src_dir.rglob('*.rs'))
    
    total_replacements = 0
    for filepath in files:
        replacements = process_file(filepath, dry_run=args.dry_run)
        total_replacements += replacements
    
    print(f"\nTotal: {total_replacements} replacements" + (" (dry run)" if args.dry_run else ""))
    
    if args.dry_run and total_replacements > 0:
        print("\nRun without --dry-run to apply changes")


if __name__ == '__main__':
    main()
