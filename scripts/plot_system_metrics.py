#!/usr/bin/env python3
"""
Plot system metrics from either DuckDB or SQLite database.

Usage:
  python plot_system_metrics.py --db /path/to/database.db [--pid PID] [--limit LIMIT]
  python plot_system_metrics.py --db ~/.kronical/system-tracker.sqlite3

Options:
  --db, -d      Path to database file (required)
  --pid, -p     Process ID to filter by (default: all PIDs)
  --limit, -l   Limit number of data points to plot (default: 1000)
  --help, -h    Show this help message
"""

import argparse
import os
import sys
from pathlib import Path
from typing import Optional, List, Tuple

import pandas as pd
import matplotlib.pyplot as plt
import matplotlib.dates as mdates
from datetime import datetime, timezone


def detect_database_type(db_path: str) -> str:
    """Detect whether the database is DuckDB or SQLite."""
    path = Path(db_path)
    if not path.exists():
        raise FileNotFoundError(f"Database file not found: {db_path}")
    
    # Check file extension first
    if path.suffix.lower() in ['.duckdb', '.ddb']:
        return 'duckdb'
    elif path.suffix.lower() in ['.sqlite', '.sqlite3', '.db']:
        return 'sqlite'
    
    # Try to detect by reading file header
    with open(db_path, 'rb') as f:
        header = f.read(16)
        if header.startswith(b'SQLite format 3'):
            return 'sqlite'
        # DuckDB doesn't have a specific header we can easily detect
        # so we'll try to connect and see what works
    
    # Default to trying SQLite first, then DuckDB
    try:
        import sqlite3
        conn = sqlite3.connect(db_path)
        conn.execute("SELECT name FROM sqlite_master WHERE type='table' LIMIT 1")
        conn.close()
        return 'sqlite'
    except:
        try:
            import duckdb
            conn = duckdb.connect(db_path)
            conn.execute("SELECT * FROM information_schema.tables LIMIT 1")
            conn.close()
            return 'duckdb'
        except:
            raise ValueError("Could not determine database type. Please specify with --type")


def query_sqlite_metrics(db_path: str, pid: Optional[int] = None, limit: int = 1000) -> pd.DataFrame:
    """Query system metrics from SQLite database."""
    import sqlite3
    
    conn = sqlite3.connect(db_path)
    
    query = """
    SELECT timestamp, pid, cpu_percent, memory_bytes, disk_io_bytes
    FROM system_metrics
    """
    
    params = []
    if pid is not None:
        query += " WHERE pid = ?"
        params.append(pid)
    
    query += " ORDER BY timestamp ASC"
    
    if limit:
        query += " LIMIT ?"
        params.append(limit)
    
    df = pd.read_sql_query(query, conn, params=params)
    conn.close()
    
    # Convert timestamp string to datetime
    df['timestamp'] = pd.to_datetime(df['timestamp'], utc=True)
    
    return df


def query_duckdb_metrics(db_path: str, pid: Optional[int] = None, limit: int = 1000) -> pd.DataFrame:
    """Query system metrics from DuckDB database."""
    import duckdb
    
    conn = duckdb.connect(db_path)
    
    query = """
    SELECT timestamp, pid, cpu_percent, memory_bytes, disk_io_bytes
    FROM system_metrics
    """
    
    params = []
    if pid is not None:
        query += " WHERE pid = ?"
        params.append(pid)
    
    query += " ORDER BY timestamp ASC"
    
    if limit:
        query += " LIMIT ?"
        params.append(limit)
    
    result = conn.execute(query, params)
    df = result.fetchdf()
    conn.close()
    
    return df


def human_bytes(n: float) -> str:
    """Convert a byte count to a human-readable string."""
    units = ["B", "KB", "MB", "GB", "TB", "PB"]
    i = 0
    v = float(n)
    while v >= 1024.0 and i < len(units) - 1:
        v /= 1024.0
        i += 1
    if v >= 100:
        return f"{v:,.0f} {units[i]}"
    elif v >= 10:
        return f"{v:,.1f} {units[i]}"
    else:
        return f"{v:,.2f} {units[i]}"


def plot_metrics(df: pd.DataFrame, title: str = "System Metrics"):
    """Plot CPU, memory, and disk IO metrics with better readability for spiky series."""
    if df.empty:
        print("No data to plot")
        return

    # Ensure datetime index
    df = df.sort_values("timestamp").copy()
    df.set_index("timestamp", inplace=True)
    # drop the first 10 rows
    df = df.iloc[60:]

    # Light smoothing to reveal trend without hiding spikes
    # Using time-based rolling windows so it works with uneven sampling
    cpu_sm = df["cpu_percent"].rolling("5s", min_periods=1).mean()
    disk_sm = df["disk_io_bytes"].rolling("5s", min_periods=1).mean()

    # Helpful percentiles for axis clipping (keeps rare spikes from flattening everything)
    cpu_p99 = df["cpu_percent"].quantile(0.99) if not df["cpu_percent"].empty else 0
    disk_p99 = df["disk_io_bytes"].quantile(0.99) if not df["disk_io_bytes"].empty else 0

    # Precompute memory in MB and its rate of change (bytes/sec)
    mem_mb = df["memory_bytes"] / (1024 ** 2)
    # Use diff over elapsed seconds to get a rate that is robust to uneven intervals
    elapsed = (mem_mb.index.to_series().diff().dt.total_seconds()).fillna(0.0)
    mem_rate_bps = df["memory_bytes"].diff().fillna(0.0) / elapsed.replace(0, pd.NA)
    mem_rate_bps = mem_rate_bps.fillna(0.0).rolling("10s", min_periods=1).mean()

    import matplotlib.pyplot as plt
    import matplotlib.dates as mdates
    from matplotlib.ticker import FuncFormatter

    plt.close("all")
    plt.rcParams["figure.figsize"] = (12, 10)
    plt.rcParams["axes.grid"] = True

    fig, (ax1, ax2, ax3) = plt.subplots(3, 1, sharex=True)

    # ===== CPU =====
    ax1.plot(cpu_sm.index, cpu_sm.values, linewidth=1.8)
    # Light dots only where CPU is non-zero to hint at activity
    nz_cpu = df["cpu_percent"] > 0
    ax1.scatter(df.index[nz_cpu], df["cpu_percent"][nz_cpu], s=6, alpha=0.25)
    ax1.set_ylabel("CPU %")
    ax1.set_title("CPU Utilization")
    if cpu_p99 > 0:
        ax1.set_ylim(0, max(1.0, cpu_p99))
        max_cpu = df["cpu_percent"].max()
        if max_cpu > cpu_p99:
            ax1.annotate(
                f"clipped above p99={cpu_p99:.2f}% (max {max_cpu:.2f}%)",
                xy=(1, 1), xycoords="axes fraction",
                xytext=(-5, -5), textcoords="offset points",
                ha="right", va="top", fontsize=9, alpha=0.7
            )

    # ===== Memory (primary: MB) with secondary axis: delta bytes/sec =====
    ax2.plot(mem_mb.index, mem_mb.values, linewidth=1.8)
    ax2.set_ylabel("Memory (MB)")
    ax2.set_title("Memory Consumption")

    ax2b = ax2.twinx()
    ax2b.plot(mem_rate_bps.index, mem_rate_bps.values, linewidth=1.0, alpha=0.6)
    ax2b.set_ylabel("Δ Memory (B/s)")
    ax2b.yaxis.set_major_formatter(FuncFormatter(lambda v, _: human_bytes(v)))

    # Format memory ticks as MB with at most one decimal
    ax2.yaxis.set_major_formatter(FuncFormatter(lambda v, _: f"{v:.1f}"))

    # ===== Disk I/O =====
    # Symlog shows detail near zero while keeping spikes visible
    ax3.set_yscale("symlog", linthresh=1e3)  # 1 KB linear region around zero
    ax3.plot(disk_sm.index, disk_sm.values, linewidth=1.8)
    # Mark non-zero I/O events
    nz_disk = df["disk_io_bytes"] > 0
    ax3.scatter(df.index[nz_disk], df["disk_io_bytes"][nz_disk], s=6, alpha=0.25)
    ax3.set_ylabel("Disk I/O (bytes)")
    ax3.set_xlabel("Time")
    ax3.set_title("Disk I/O Activity")
    ax3.yaxis.set_major_formatter(FuncFormatter(lambda v, _: human_bytes(v)))

    # Optional clipping annotation (the scale is symlog so clipping is rarely needed,
    # but we still show if p99 is far below max)
    if disk_p99 > 0:
        max_io = df["disk_io_bytes"].max()
        if max_io > 10 * max(disk_p99, 1):  # very extreme spikes
            ax3.annotate(
                f"extreme spikes (max {human_bytes(max_io)})",
                xy=(1, 1), xycoords="axes fraction",
                xytext=(-5, -5), textcoords="offset points",
                ha="right", va="top", fontsize=9, alpha=0.7
            )

    # ===== Shared X formatting =====
    for ax in (ax1, ax2, ax3):
        ax.grid(True, alpha=0.3)
        ax.xaxis.set_major_formatter(mdates.DateFormatter("%H:%M:%S"))
        ax.xaxis.set_major_locator(mdates.AutoDateLocator(maxticks=8))

    plt.suptitle(title, fontsize=14)
    plt.tight_layout()
    plt.show()


def main():
    parser = argparse.ArgumentParser(description="Plot system metrics from DuckDB or SQLite database")
    parser.add_argument("--db", "-d", required=True, help="Path to database file")
    parser.add_argument("--pid", "-p", type=int, help="Process ID to filter by")
    parser.add_argument("--limit", "-l", type=int, default=10000, help="Limit number of data points")
    parser.add_argument("--type", "-t", choices=['auto', 'duckdb', 'sqlite'], default='auto', 
                       help="Database type (auto-detected if not specified)")
    
    args = parser.parse_args()
    
    if not os.path.exists(args.db):
        print(f"Error: Database file not found: {args.db}")
        sys.exit(1)
    
    try:
        # Detect database type
        if args.type == 'auto':
            db_type = detect_database_type(args.db)
        else:
            db_type = args.type
        
        print(f"Detected database type: {db_type}")
        
        # Query data
        if db_type == 'sqlite':
            df = query_sqlite_metrics(args.db, args.pid, args.limit)
        else:  # duckdb
            df = query_duckdb_metrics(args.db, args.pid, args.limit)
        
        if df.empty:
            print("No metrics data found in the database")
            sys.exit(1)
        
        print(f"Retrieved {len(df)} data points")
        print(f"Time range: {df['timestamp'].min()} to {df['timestamp'].max()}")
        
        if args.pid:
            pids = df['pid'].unique()
            print(f"PIDs in data: {pids}")
        
        # Create title
        title = "System Metrics"
        if args.pid:
            title += f" (PID: {args.pid})"
        
        # Plot the data
        plot_metrics(df, title)
        
    except Exception as e:
        print(f"Error: {e}")
        sys.exit(1)


if __name__ == "__main__":
    main()
