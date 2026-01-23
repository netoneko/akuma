#!/usr/bin/env python3
"""
Analyze kernel crash logs for context switch irregularities.

Usage: python3 analyze_crash.py crash132.log
"""

import re
import sys
from collections import defaultdict
from dataclasses import dataclass
from typing import Optional, List, Tuple

@dataclass
class SwitchEvent:
    line_num: int
    seq: Optional[int]
    event_type: str  # 'switching', 'back', 'returned', 'irq_entry', 'irq_exit'
    from_tid: Optional[int] = None
    to_tid: Optional[int] = None
    tid: Optional[int] = None  # For IRQ events
    sp_now: Optional[int] = None  # Actual SP register before switch
    old_sp: Optional[int] = None  # Previously saved context.sp
    new_sp: Optional[int] = None  # Context.sp being loaded
    new_elr: Optional[int] = None  # ELR being loaded (new format)
    sp: Optional[int] = None
    x30: Optional[int] = None
    elr: Optional[int] = None
    tpidr: Optional[int] = None
    raw_line: str = ""

@dataclass
class ExceptionInfo:
    line_num: int
    ec: int
    iss: int
    elr: int
    far: int
    spsr: int
    thread: int
    sp: int
    raw_lines: str = ""

def parse_hex(s: str) -> int:
    """Parse hex string like '0x41fffc60' to int."""
    return int(s, 16)

def parse_log(filename: str) -> Tuple[List[SwitchEvent], Optional[ExceptionInfo]]:
    """Parse the crash log file."""
    events = []
    exception = None
    
    # Patterns
    switching_pattern = re.compile(
        r'\[SGI\] switching (\d+) -> (\d+)'
    )
    # New format: SP_now=X new_ctx.sp=Y new_ctx.elr=Z
    ctx_new_pattern = re.compile(
        r'SP_now=(0x[0-9a-f]+)\s+new_ctx\.sp=(0x[0-9a-f]+)\s+new_ctx\.elr=(0x[0-9a-f]+)'
    )
    # Old format: SP_now=X old_ctx=Y new_ctx=Z
    ctx_sp_pattern_new = re.compile(
        r'SP_now=(0x[0-9a-f]+)\s+old_ctx=(0x[0-9a-f]+)\s+new_ctx=(0x[0-9a-f]+)'
    )
    # Very old format: old_ctx.sp=X new_ctx.sp=Y
    ctx_sp_pattern = re.compile(
        r'old_ctx\.sp=(0x[0-9a-f]+)\s+new_ctx\.sp=(0x[0-9a-f]+)'
    )
    back_pattern = re.compile(
        r'\[SGI\] back, SP=(0x[0-9a-f]+)'
    )
    returned_pattern = re.compile(
        r'\[SGI\] returned to tid=(\d+) seq=(\d+) SP=(0x[0-9a-f]+) x30=(0x[0-9a-f]+) ELR=(0x[0-9a-f]+)'
    )
    irq_entry_pattern = re.compile(
        r'\[IRQ\] entry: tid=(\d+) tpidr=(0x[0-9a-f]+) sp=(0x[0-9a-f]+)'
    )
    irq_exit_pattern = re.compile(
        r'\[IRQ\] exit: tid=(\d+) tpidr=(0x[0-9a-f]+) sp=(0x[0-9a-f]+)'
    )
    exception_pattern = re.compile(
        r'\[Exception\].*EC=(0x[0-9a-f]+),\s*ISS=(0x[0-9a-f]+)'
    )
    exception_details_pattern = re.compile(
        r'ELR=(0x[0-9a-f]+),\s*FAR=(0x[0-9a-f]+),\s*SPSR=(0x[0-9a-f]+)'
    )
    exception_thread_pattern = re.compile(
        r'Thread=(\d+).*SP=(0x[0-9a-f]+)'
    )
    ctx_bug_pattern = re.compile(
        r'\[CTX BUG\]'
    )
    
    with open(filename, 'r') as f:
        lines = f.readlines()
    
    i = 0
    while i < len(lines):
        line = lines[i].strip()
        
        # Check for CTX BUG
        if ctx_bug_pattern.search(line):
            event = SwitchEvent(
                line_num=i+1,
                seq=None,
                event_type='ctx_bug',
                raw_line=line
            )
            events.append(event)
            i += 1
            continue
        
        # Check for IRQ entry
        m = irq_entry_pattern.search(line)
        if m:
            event = SwitchEvent(
                line_num=i+1,
                seq=None,
                event_type='irq_entry',
                tid=int(m.group(1)),
                tpidr=parse_hex(m.group(2)),
                sp=parse_hex(m.group(3)),
                raw_line=line
            )
            events.append(event)
            i += 1
            continue
        
        # Check for IRQ exit
        m = irq_exit_pattern.search(line)
        if m:
            event = SwitchEvent(
                line_num=i+1,
                seq=None,
                event_type='irq_exit',
                tid=int(m.group(1)),
                tpidr=parse_hex(m.group(2)),
                sp=parse_hex(m.group(3)),
                raw_line=line
            )
            events.append(event)
            i += 1
            continue
        
        # Check for switching event
        m = switching_pattern.search(line)
        if m:
            event = SwitchEvent(
                line_num=i+1,
                seq=None,
                event_type='switching',
                from_tid=int(m.group(1)),
                to_tid=int(m.group(2)),
                raw_line=line
            )
            # Check next line for SP/ELR values
            if i + 1 < len(lines):
                next_line = lines[i + 1].strip()
                # Try newest format first (with new_ctx.elr)
                m2 = ctx_new_pattern.search(next_line)
                if m2:
                    event.sp_now = parse_hex(m2.group(1))
                    event.new_sp = parse_hex(m2.group(2))
                    event.new_elr = parse_hex(m2.group(3))
                    event.raw_line += "\n  " + next_line
                    i += 1
                else:
                    # Try old format with SP_now, old_ctx, new_ctx
                    m2 = ctx_sp_pattern_new.search(next_line)
                    if m2:
                        event.sp_now = parse_hex(m2.group(1))
                        event.old_sp = parse_hex(m2.group(2))
                        event.new_sp = parse_hex(m2.group(3))
                        event.raw_line += "\n  " + next_line
                        i += 1
                    else:
                        # Try oldest format
                        m2 = ctx_sp_pattern.search(next_line)
                        if m2:
                            event.old_sp = parse_hex(m2.group(1))
                            event.new_sp = parse_hex(m2.group(2))
                            event.raw_line += "\n  " + next_line
                            i += 1
            events.append(event)
            i += 1
            continue
        
        # Check for back event
        m = back_pattern.search(line)
        if m:
            event = SwitchEvent(
                line_num=i+1,
                seq=None,
                event_type='back',
                sp=parse_hex(m.group(1)),
                raw_line=line
            )
            events.append(event)
            i += 1
            continue
        
        # Check for returned event
        m = returned_pattern.search(line)
        if m:
            event = SwitchEvent(
                line_num=i+1,
                seq=int(m.group(2)),
                event_type='returned',
                to_tid=int(m.group(1)),
                sp=parse_hex(m.group(3)),
                x30=parse_hex(m.group(4)),
                elr=parse_hex(m.group(5)),
                raw_line=line
            )
            events.append(event)
            i += 1
            continue
        
        # Check for exception
        m = exception_pattern.search(line)
        if m:
            exc_info = ExceptionInfo(
                line_num=i+1,
                ec=parse_hex(m.group(1)),
                iss=parse_hex(m.group(2)),
                elr=0, far=0, spsr=0, thread=0, sp=0,
                raw_lines=line
            )
            # Parse following lines for details
            for j in range(i+1, min(i+10, len(lines))):
                detail_line = lines[j].strip()
                exc_info.raw_lines += "\n  " + detail_line
                
                m2 = exception_details_pattern.search(detail_line)
                if m2:
                    exc_info.elr = parse_hex(m2.group(1))
                    exc_info.far = parse_hex(m2.group(2))
                    exc_info.spsr = parse_hex(m2.group(3))
                
                m2 = exception_thread_pattern.search(detail_line)
                if m2:
                    exc_info.thread = int(m2.group(1))
                    exc_info.sp = parse_hex(m2.group(2))
            
            exception = exc_info
            event = SwitchEvent(
                line_num=i+1,
                seq=None,
                event_type='exception',
                raw_line=line
            )
            events.append(event)
            i += 1
            continue
        
        i += 1
    
    return events, exception

def analyze_events(events: list, exception: Optional[ExceptionInfo], output_file=None):
    """Analyze events for irregularities."""
    def out(s):
        print(s)
        if output_file:
            output_file.write(s + "\n")
    
    out("=" * 70)
    out("CRASH LOG ANALYSIS")
    out("=" * 70)
    
    # Check for CTX BUG messages
    ctx_bugs = [e for e in events if e.event_type == 'ctx_bug']
    if ctx_bugs:
        out("\n*** CONTEXT BUGS DETECTED ***")
        for bug in ctx_bugs:
            out(f"  Line {bug.line_num}: {bug.raw_line}")
    
    # Report exception info
    if exception:
        out("\n" + "-" * 70)
        out("EXCEPTION DETAILS")
        out("-" * 70)
        ec_names = {
            0x00: "Unknown/Undefined",
            0x0e: "Illegal Execution State",
            0x21: "Instruction Abort (EL1)",
            0x22: "PC Alignment",
            0x25: "Data Abort (EL1)",
        }
        ec_name = ec_names.get(exception.ec, f"EC={exception.ec:#x}")
        out(f"  Type: {ec_name}")
        out(f"  ELR: {exception.elr:#x} (return address)")
        out(f"  FAR: {exception.far:#x} (fault address)")
        out(f"  SPSR: {exception.spsr:#x}")
        out(f"  Thread: {exception.thread}")
        out(f"  SP: {exception.sp:#x}")
        
        if exception.elr == 0:
            out("\n  *** ELR=0 BUG: Tried to execute at address 0! ***")
        if exception.far < 0x40000000 and exception.ec == 0x25:
            out(f"\n  *** Data Abort accessing user-space address {exception.far:#x} ***")
    
    # Track per-thread state
    thread_sps = defaultdict(list)  # tid -> list of (seq, sp)
    thread_elrs = defaultdict(list)  # tid -> list of (seq, elr)
    ctx_elrs = defaultdict(list)  # tid -> list of new_ctx.elr values
    irq_sps = defaultdict(list)  # tid -> list of IRQ SP values
    
    current_seq = 0
    
    for event in events:
        if event.seq:
            current_seq = event.seq
        
        if event.event_type == 'switching':
            if event.new_elr is not None:
                ctx_elrs[event.to_tid].append((current_seq, event.new_elr))
        
        elif event.event_type == 'returned':
            tid = event.to_tid
            thread_sps[tid].append((event.seq, event.sp))
            thread_elrs[tid].append((event.seq, event.elr))
        
        elif event.event_type in ('irq_entry', 'irq_exit'):
            irq_sps[event.tid].append((current_seq, event.sp))
    
    # Report statistics
    out(f"\nTotal events: {len(events)}")
    switch_count = sum(1 for e in events if e.event_type == 'switching')
    out(f"Context switches: {switch_count}")
    out(f"Threads observed: {sorted(set(thread_sps.keys()) | set(ctx_elrs.keys()))}")
    
    # Check for ELR=0 in new_ctx
    out("\n" + "-" * 70)
    out("ELR VALUE ANALYSIS (new_ctx.elr at switch time)")
    out("-" * 70)
    
    elr_zero_count = 0
    for tid in sorted(ctx_elrs.keys()):
        elrs = ctx_elrs[tid]
        zeros = [(seq, elr) for seq, elr in elrs if elr == 0]
        if zeros:
            elr_zero_count += len(zeros)
            out(f"\n  *** Thread {tid} has ELR=0 at {len(zeros)} switch(es)! ***")
            for seq, _ in zeros[:5]:
                out(f"      seq={seq}")
        
        unique_elrs = set(elr for _, elr in elrs)
        if len(unique_elrs) <= 5:
            out(f"\nThread {tid} new_ctx.elr values:")
            for elr in sorted(unique_elrs):
                count = sum(1 for _, e in elrs if e == elr)
                out(f"  {elr:#x}: {count} times")
    
    if elr_zero_count == 0:
        out("\n  No ELR=0 bugs detected in context switches!")
    
    # Analyze returned ELR values
    out("\n" + "-" * 70)
    out("RETURNED ELR ANALYSIS (after switch_context returns)")
    out("-" * 70)
    
    for tid in sorted(thread_elrs.keys()):
        elrs = thread_elrs[tid]
        if len(elrs) < 2:
            continue
        
        # Check for ELR=0
        zeros = [(seq, elr) for seq, elr in elrs if elr == 0]
        if zeros:
            out(f"\n  *** Thread {tid} returned with ELR=0 at {len(zeros)} time(s)! ***")
        
        unique_elrs = set(elr for _, elr in elrs)
        if len(unique_elrs) <= 5:
            out(f"\nThread {tid} returned ELR values:")
            for elr in sorted(unique_elrs):
                count = sum(1 for _, e in elrs if e == elr)
                out(f"  {elr:#x}: {count} times")
    
    # Analyze IRQ SP consistency
    out("\n" + "-" * 70)
    out("IRQ SP ANALYSIS")
    out("-" * 70)
    
    for tid in sorted(irq_sps.keys()):
        sps = irq_sps[tid]
        unique_sps = set(sp for _, sp in sps)
        out(f"\nThread {tid} IRQ SP values:")
        for sp in sorted(unique_sps):
            count = sum(1 for _, s in sps if s == sp)
            out(f"  {sp:#x}: {count} times")
    
    # Show events leading up to crash
    out("\n" + "-" * 70)
    out("LAST 40 EVENTS BEFORE CRASH")
    out("-" * 70)
    
    for event in events[-40:]:
        if event.event_type == 'switching':
            out(f"  [{event.line_num}] SWITCH {event.from_tid} -> {event.to_tid}")
            if event.new_elr is not None:
                out(f"         SP_now={event.sp_now:#x}  new_ctx.sp={event.new_sp:#x}  new_ctx.elr={event.new_elr:#x}")
            elif event.sp_now is not None:
                out(f"         SP_now={event.sp_now:#x}  old_ctx={event.old_sp:#x}  new_ctx={event.new_sp:#x}")
        elif event.event_type == 'back':
            out(f"  [{event.line_num}] BACK SP={event.sp:#x}")
        elif event.event_type == 'returned':
            out(f"  [{event.line_num}] RETURNED tid={event.to_tid} seq={event.seq}")
            out(f"         SP={event.sp:#x}  x30={event.x30:#x}  ELR={event.elr:#x}")
        elif event.event_type == 'irq_entry':
            out(f"  [{event.line_num}] IRQ_ENTRY tid={event.tid} sp={event.sp:#x}")
        elif event.event_type == 'irq_exit':
            out(f"  [{event.line_num}] IRQ_EXIT tid={event.tid} sp={event.sp:#x}")
        elif event.event_type == 'exception':
            out(f"  [{event.line_num}] *** EXCEPTION ***")
        elif event.event_type == 'ctx_bug':
            out(f"  [{event.line_num}] *** CTX BUG: {event.raw_line}")

def main():
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <crash_log_file> [output_file]")
        sys.exit(1)
    
    filename = sys.argv[1]
    output_filename = sys.argv[2] if len(sys.argv) > 2 else None
    
    events, exception = parse_log(filename)
    
    if not events:
        print("No events found in log file.")
        sys.exit(1)
    
    output_file = None
    if output_filename:
        output_file = open(output_filename, 'w')
    
    try:
        analyze_events(events, exception, output_file)
    finally:
        if output_file:
            output_file.close()
            print(f"Analysis saved to {output_filename}")

if __name__ == "__main__":
    main()
