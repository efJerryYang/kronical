#!/usr/bin/env python3
"""
Live plot for system-info.log (CSV):
# ts,cpu_pct,rss_bytes,disk_io_bytes
2025-08-30T22:37:39Z,0.1,21725184,0
...

Requirements:
  pip install matplotlib pandas

Usage:
  python monitor_plot.py --file system-info.log --window 600
"""

import argparse
import os
import sys
from typing import Optional

import pandas as pd
import matplotlib.pyplot as plt
import matplotlib.dates as mdates
from matplotlib.animation import FuncAnimation


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


def load_csv(path: str, last_n: Optional[int]) -> pd.DataFrame:
    """Load the CSV, parse timestamps, keep only the last N rows if requested."""
    try:
        df = pd.read_csv(
            path,
            comment="#",
            names=["ts", "cpu_pct", "rss_bytes", "disk_io_bytes"],
            header=None,
            skip_blank_lines=True,
            engine="c",
        )
    except Exception:
        df = pd.read_csv(
            path,
            comment="#",
            names=["ts", "cpu_pct", "rss_bytes", "disk_io_bytes"],
            header=None,
            skip_blank_lines=True,
            engine="python",
        )

    df = df.dropna(how="all")
    df = df[df["ts"].notna()].copy()

    # Parse timestamp (ISO 8601 with Z)
    df["ts"] = pd.to_datetime(df["ts"], utc=True, errors="coerce")
    df = df.dropna(subset=["ts"]).sort_values("ts")

    # Numeric columns
    for col in ("cpu_pct", "rss_bytes", "disk_io_bytes"):
        df[col] = pd.to_numeric(df[col], errors="coerce")
    df = df.dropna(subset=["cpu_pct", "rss_bytes", "disk_io_bytes"])

    if last_n is not None and len(df) > last_n:
        df = df.iloc[-last_n:].copy()

    # Compute disk throughput (bytes per second) from cumulative bytes
    df["disk_bps"] = df["disk_io_bytes"].diff()
    dt = df["ts"].diff().dt.total_seconds()
    valid = dt > 0
    df.loc[valid, "disk_bps"] = df.loc[valid, "disk_bps"] / dt[valid]
    df.loc[df["disk_bps"] < 0, "disk_bps"] = pd.NA

    return df


def main():
    ap = argparse.ArgumentParser(description="Live plot system-info.log")
    ap.add_argument("--file", "-f", required=True, help="Path to system-info.log")
    ap.add_argument(
        "--window", "-w", type=int, default=600,
        help="Number of most recent points to display (default: 600)"
    )
    ap.add_argument(
        "--interval", "-i", type=int, default=1000,
        help="Refresh interval in ms (default: 1000)"
    )
    args = ap.parse_args()

    path = args.file
    if not os.path.exists(path):
        print(f"Error: file not found: {path}", file=sys.stderr)
        sys.exit(1)

    # Matplotlib setup
    plt.close("all")
    plt.rcParams["figure.figsize"] = (11, 7)
    plt.rcParams["axes.grid"] = True

    fig = plt.figure(constrained_layout=True)
    gs = fig.add_gridspec(3, 1, height_ratios=[1, 1, 1])

    ax_cpu = fig.add_subplot(gs[0, 0])
    ax_mem = fig.add_subplot(gs[1, 0])
    ax_io = fig.add_subplot(gs[2, 0])

    ax_cpu.set_title("CPU Utilization (%)")
    ax_mem.set_title("RSS (Resident Memory)")
    ax_io.set_title("Disk Throughput (bytes/sec)")

    for ax in (ax_cpu, ax_mem, ax_io):
        ax.xaxis.set_major_formatter(mdates.DateFormatter("%H:%M:%S"))
        ax.xaxis.set_major_locator(mdates.AutoDateLocator(maxticks=8))
        ax.tick_params(axis="x", rotation=0)

    # Initialize empty lines
    cpu_line, = ax_cpu.plot([], [], linewidth=2)
    mem_line, = ax_mem.plot([], [], linewidth=2)
    io_line, = ax_io.plot([], [], linewidth=2)

    last_mtime = 0.0

    def on_key(event):
        if event.key == "q":
            plt.close(fig)
    fig.canvas.mpl_connect("key_press_event", on_key)

    def update(_frame):
        nonlocal last_mtime
        try:
            mtime = os.path.getmtime(path)
        except OSError:
            return cpu_line, mem_line, io_line

        if mtime == last_mtime and cpu_line.get_xdata().size > 0:
            # Nothing new; just keep the canvas alive
            fig.canvas.draw_idle()
            return cpu_line, mem_line, io_line

        last_mtime = mtime

        df = load_csv(path, last_n=args.window)
        if df.empty:
            return cpu_line, mem_line, io_line

        # Use the datetime Series directly; Matplotlib understands it
        x = df["ts"]

        # CPU %
        cpu_line.set_data(x, df["cpu_pct"])
        ax_cpu.set_xlim(x.iloc[0], x.iloc[-1])
        ax_cpu.relim()
        ax_cpu.autoscale(axis="y", tight=False)

        # Memory (bytes)
        mem_series = df["rss_bytes"]
        mem_line.set_data(x, mem_series)
        ax_mem.set_xlim(x.iloc[0], x.iloc[-1])
        ax_mem.relim()
        ax_mem.autoscale(axis="y", tight=False)
        ax_mem.set_ylabel(f"{human_bytes(mem_series.iloc[-1])} now")

        # Disk throughput (bytes/s)
        io_series = df["disk_bps"].astype("float64")
        io_line.set_data(x, io_series)
        ax_io.set_xlim(x.iloc[0], x.iloc[-1])
        ax_io.relim()
        ax_io.autoscale(axis="y", tight=False)

        # Titles with latest values
        ax_cpu.set_title(f"CPU Utilization (%) — now {df['cpu_pct'].iloc[-1]:.1f}%")
        latest_bps = io_series.iloc[-1]
        ax_io.set_title(
            f"Disk Throughput (bytes/sec) — now {human_bytes(latest_bps)}/s"
            if pd.notna(latest_bps) else "Disk Throughput (bytes/sec)"
        )

        fig.autofmt_xdate(rotation=0)
        return cpu_line, mem_line, io_line

    # cache_frame_data=False removes the warning and avoids unbounded cache
    ani = FuncAnimation(fig, update, interval=args.interval, blit=False, cache_frame_data=False)

    # Draw the first frame explicitly
    update(0)
    plt.suptitle(os.path.basename(path), fontsize=11)
    plt.show()


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass

