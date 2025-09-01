#!/usr/bin/env python3
"""
Unified system monitoring and plotting script for Chronicle daemon.

Features:
- Track CPU, memory, and disk I/O with microsecond precision timestamps
- Live plotting with --watch mode
- Lock file mechanism to prevent concurrent tracking
- CSV output with automatic compression
- Auto-detection of Chronicle daemon PID

Usage:
  python tracker.py [PID]              # Track specified PID or auto-detect
  python tracker.py --watch [logfile]  # Live plot existing log file
"""

import argparse
import atexit
import fcntl
import os
import signal
import subprocess
import sys
import time
import zipfile
import csv
import io
from datetime import datetime, timezone, timedelta
from pathlib import Path
from typing import Optional, Tuple

import pandas as pd
import matplotlib.pyplot as plt
import matplotlib.dates as mdates
from matplotlib.animation import FuncAnimation


class ProcessTracker:
    def __init__(self, pid: int, zip_file: str = "system-info.zip", interval: float = 1.0):
        self.pid = pid
        self.zip_file = zip_file
        self.csv_name = "system-info.csv"
        self.interval = interval
        self.lock_file = f"{zip_file}.lock"
        self.lock_fd = None
        self.pidio_bin = self._find_pidio()
        self.max_records = 1440 * 60 * 24
        
    def _find_pidio(self) -> str:
        script_dir = Path(__file__).parent
        pidio_path = script_dir / "pidio"
        
        if pidio_path.exists() and os.access(pidio_path, os.X_OK):
            return str(pidio_path)
            
        if subprocess.run(["which", "pidio"], capture_output=True).returncode == 0:
            return "pidio"
            
        pidio_c = script_dir / "pidio.c"
        if pidio_c.exists():
            print("Compiling pidio helper...", file=sys.stderr)
            subprocess.run([
                "clang", "-O2", "-Wall", "-Wextra", 
                "-o", str(pidio_path), str(pidio_c)
            ], check=True)
            return str(pidio_path)
            
        raise RuntimeError("pidio helper not found and cannot be compiled")
    
    def acquire_lock(self) -> bool:
        try:
            self.lock_fd = open(self.lock_file, 'w')
            fcntl.flock(self.lock_fd.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.lock_fd.write(str(os.getpid()))
            self.lock_fd.flush()
            atexit.register(self.release_lock)
            return True
        except (IOError, OSError):
            if self.lock_fd:
                self.lock_fd.close()
                self.lock_fd = None
            return False
    
    def release_lock(self):
        if self.lock_fd:
            try:
                fcntl.flock(self.lock_fd.fileno(), fcntl.LOCK_UN)
                self.lock_fd.close()
            except:
                pass
            self.lock_fd = None
        try:
            os.unlink(self.lock_file)
        except:
            pass
    
    def get_system_stats(self) -> Tuple[float, int, int]:
        try:
            result = subprocess.run([
                "ps", "-o", "%cpu=", "-o", "rss=", "-p", str(self.pid)
            ], capture_output=True, text=True, timeout=5)
            
            if result.returncode != 0:
                return 0.0, 0, 0
                
            lines = result.stdout.strip().split('\n')
            if not lines:
                return 0.0, 0, 0
                
            parts = lines[0].split()
            cpu_pct = float(parts[0]) if parts[0] else 0.0
            rss_kb = int(parts[1]) if parts[1] else 0
            rss_bytes = rss_kb * 1024
            
        except (subprocess.TimeoutExpired, ValueError, IndexError):
            cpu_pct, rss_bytes = 0.0, 0
            
        try:
            result = subprocess.run([
                self.pidio_bin, str(self.pid)
            ], capture_output=True, text=True, timeout=5)
            
            if result.returncode == 0:
                parts = result.stdout.strip().split()
                if len(parts) >= 2:
                    read_bytes = int(parts[0])
                    write_bytes = int(parts[1])
                    disk_io_bytes = read_bytes + write_bytes
                else:
                    disk_io_bytes = 0
            else:
                disk_io_bytes = 0
                
        except (subprocess.TimeoutExpired, ValueError, IndexError):
            disk_io_bytes = 0
            
        return cpu_pct, rss_bytes, disk_io_bytes
    
    def load_existing_data(self) -> list:
        if not Path(self.zip_file).exists():
            return [["ts", "cpu_pct", "rss_bytes", "disk_io_bytes"]]
        
        try:
            with zipfile.ZipFile(self.zip_file, 'r') as zf:
                with zf.open(self.csv_name) as csvfile:
                    reader = csv.reader(io.TextIOWrapper(csvfile, encoding='utf-8'))
                    return list(reader)
        except (zipfile.BadZipFile, KeyError):
            return [["ts", "cpu_pct", "rss_bytes", "disk_io_bytes"]]
    
    def save_data(self, data: list):
        csv_buffer = io.StringIO()
        writer = csv.writer(csv_buffer)
        writer.writerows(data)
        csv_content = csv_buffer.getvalue().encode('utf-8')
        
        with zipfile.ZipFile(self.zip_file, 'w', zipfile.ZIP_DEFLATED) as zf:
            zf.writestr(self.csv_name, csv_content)
    
    def process_exists(self) -> bool:
        try:
            os.kill(self.pid, 0)
            return True
        except OSError:
            return False
    
    def track(self):
        if not self.acquire_lock():
            print(f"Another tracker instance is already running for {self.zip_file}")
            print(f"Use: python {sys.argv[0]} --watch {self.zip_file} to view live plots")
            sys.exit(1)
            
        existing_data = self.load_existing_data()
        
        print(f"Tracking PID {self.pid} -> {self.zip_file} (interval: {self.interval}s)")
        print("Press Ctrl+C to stop")
        
        def signal_handler(signum, frame):
            print("\nStopping tracker...")
            sys.exit(0)
            
        signal.signal(signal.SIGINT, signal_handler)
        signal.signal(signal.SIGTERM, signal_handler)
        
        try:
            while self.process_exists():
                timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
                cpu_pct, rss_bytes, disk_io_bytes = self.get_system_stats()
                
                new_row = [timestamp, cpu_pct, rss_bytes, disk_io_bytes]
                existing_data.append(new_row)
                
                if len(existing_data) > self.max_records:
                    existing_data = existing_data[-self.max_records:]
                
                self.save_data(existing_data)
                time.sleep(self.interval)
                
        except KeyboardInterrupt:
            signal_handler(signal.SIGINT, None)
            
        print(f"Process {self.pid} has exited")
    


class LivePlotter:
    def __init__(self, zip_file: str, period: str = "day", interval_ms: int = 1000):
        self.zip_file = zip_file
        self.csv_name = "system-info.csv"
        self.period = period
        self.interval_ms = interval_ms
        self.last_mtime = 0.0
        self.resolution_map = {
            "hour": 1,
            "day": 24,
            "week": 7 * 24,
            "month": 30 * 7 * 24
        }
        
    def human_bytes(self, n: float) -> str:
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
    
    def load_data(self) -> pd.DataFrame:
        try:
            if not Path(self.zip_file).exists():
                return pd.DataFrame()
            
            with zipfile.ZipFile(self.zip_file, 'r') as zf:
                with zf.open(self.csv_name) as csvfile:
                    df = pd.read_csv(io.TextIOWrapper(csvfile, encoding='utf-8'))
            
            df = df.dropna(how="all")
            df = df[df["ts"].notna()].copy()
            
            df["ts"] = pd.to_datetime(df["ts"], utc=True, errors="coerce")
            df = df.dropna(subset=["ts"]).sort_values("ts")
            
            for col in ("cpu_pct", "rss_bytes", "disk_io_bytes"):
                df[col] = pd.to_numeric(df[col], errors="coerce")
            df = df.dropna(subset=["cpu_pct", "rss_bytes", "disk_io_bytes"])
            
            cutoff_time = datetime.now(timezone.utc) - timedelta(
                seconds=self.resolution_map[self.period] * 3600
            )
            df = df[df["ts"] >= cutoff_time]
            
            resolution_seconds = self.resolution_map[self.period]
            if resolution_seconds > 1:
                df = df.iloc[::resolution_seconds].copy()
            
            df["disk_bps"] = df["disk_io_bytes"].diff()
            dt = df["ts"].diff().dt.total_seconds()
            valid = dt > 0
            df.loc[valid, "disk_bps"] = df.loc[valid, "disk_bps"] / dt[valid]
            df.loc[df["disk_bps"] < 0, "disk_bps"] = pd.NA
            
            return df
        except Exception as e:
            print(f"Error loading data: {e}", file=sys.stderr)
            return pd.DataFrame()
    
    def plot(self):
        plt.close("all")
        plt.rcParams["figure.figsize"] = (12, 8)
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
        
        cpu_line, = ax_cpu.plot([], [], linewidth=2, color='red')
        mem_line, = ax_mem.plot([], [], linewidth=2, color='blue')
        io_line, = ax_io.plot([], [], linewidth=2, color='green')
        
        def on_key(event):
            if event.key == "q":
                plt.close(fig)
        fig.canvas.mpl_connect("key_press_event", on_key)
        
        def update(_frame):
            try:
                mtime = os.path.getmtime(self.zip_file)
            except OSError:
                return cpu_line, mem_line, io_line
                
            if mtime == self.last_mtime and cpu_line.get_xdata().size > 0:
                fig.canvas.draw_idle()
                return cpu_line, mem_line, io_line
                
            self.last_mtime = mtime
            df = self.load_data()
            if df.empty or len(df) < 2:
                return cpu_line, mem_line, io_line
                
            x = df["ts"]
            
            if len(x.unique()) > 1:
                time_range = x.iloc[-1] - x.iloc[0]
                if time_range.total_seconds() > 0:
                    margin = time_range * 0.02
                    x_min, x_max = x.iloc[0] - margin, x.iloc[-1] + margin
                else:
                    x_min = x.iloc[0] - pd.Timedelta(seconds=30)
                    x_max = x.iloc[-1] + pd.Timedelta(seconds=30)
            else:
                x_min = x.iloc[0] - pd.Timedelta(seconds=30)
                x_max = x.iloc[0] + pd.Timedelta(seconds=30)
            
            cpu_line.set_data(x, df["cpu_pct"])
            ax_cpu.set_xlim(x_min, x_max)
            ax_cpu.relim()
            ax_cpu.autoscale(axis="y", tight=False)
            
            mem_series = df["rss_bytes"]
            mem_line.set_data(x, mem_series)
            ax_mem.set_xlim(x_min, x_max)
            ax_mem.relim()
            ax_mem.autoscale(axis="y", tight=False)
            ax_mem.set_ylabel(f"{self.human_bytes(mem_series.iloc[-1])} now")
            
            io_series = df["disk_bps"].astype("float64")
            io_line.set_data(x, io_series)
            ax_io.set_xlim(x_min, x_max)
            ax_io.relim()
            ax_io.autoscale(axis="y", tight=False)
            
            ax_cpu.set_title(f"CPU Utilization (%) — now {df['cpu_pct'].iloc[-1]:.1f}%")
            latest_bps = io_series.iloc[-1]
            ax_io.set_title(
                f"Disk Throughput (bytes/sec) — now {self.human_bytes(latest_bps)}/s"
                if pd.notna(latest_bps) else "Disk Throughput (bytes/sec)"
            )
            
            fig.autofmt_xdate(rotation=0)
            return cpu_line, mem_line, io_line
        
        ani = FuncAnimation(fig, update, interval=self.interval_ms, blit=False, cache_frame_data=False)
        
        update(0)
        plt.suptitle(f"Live Monitor: {Path(self.zip_file).name} ({self.period})", fontsize=12)
        plt.show()


def find_chronicle_pid() -> Optional[int]:
    try:
        result = subprocess.run(["pgrep", "kronid"], capture_output=True, text=True)
        if result.returncode == 0:
            pids = result.stdout.strip().split('\n')
            if pids and pids[0]:
                return int(pids[0])
    except:
        pass
    return None


def main():
    parser = argparse.ArgumentParser(description="System monitoring and plotting for Chronicle")
    parser.add_argument("pid", nargs="?", type=int, help="PID to monitor (auto-detect Chronicle if omitted)")
    parser.add_argument("--watch", "-w", metavar="ZIPFILE", help="Live plot mode for existing ZIP file")
    parser.add_argument("--hour", action="store_true", help="Show last hour with second resolution (use with --watch)")
    parser.add_argument("--day", action="store_true", help="Show last day with 24-second resolution (use with --watch)")
    parser.add_argument("--week", action="store_true", help="Show last week with 7*24-second resolution (use with --watch)")
    parser.add_argument("--month", action="store_true", help="Show last month with 30*7*24-second resolution (use with --watch)")
    parser.add_argument("--detect", "-d", action="store_true", help="Auto-detect and start tracking Chronicle daemon if running")
    parser.add_argument("--interval", "-i", type=float, default=1.0, help="Sampling interval in seconds (default: 1.0)")
    parser.add_argument("--output", "-o", default="system-info.zip", help="Output ZIP file (default: system-info.zip)")
    
    args = parser.parse_args()
    
    if args.watch:
        zip_file = args.watch
        if not Path(zip_file).exists():
            print(f"Error: ZIP file not found: {zip_file}", file=sys.stderr)
            sys.exit(1)
        
        period = "day"
        if args.hour:
            period = "hour"
        elif args.week:
            period = "week"
        elif args.month:
            period = "month"
        
        plotter = LivePlotter(zip_file, period)
        try:
            plotter.plot()
        except KeyboardInterrupt:
            pass
        return
    
    if args.detect:
        pid = find_chronicle_pid()
        if not pid:
            print("Chronicle daemon not running. Use 'kronictl start' to start it first.", file=sys.stderr)
            sys.exit(0)
        print(f"Detected Chronicle daemon PID: {pid} - starting tracker...")
    elif args.pid:
        pid = args.pid
    else:
        pid = find_chronicle_pid()
        if not pid:
            print("Error: No PID specified and Chronicle daemon not found", file=sys.stderr)
            print("Use 'kronictl start' to start Chronicle daemon", file=sys.stderr)
            print("Or use --detect flag to auto-start tracking when daemon is detected", file=sys.stderr)
            sys.exit(1)
        print(f"Auto-detected Chronicle daemon PID: {pid}")
    
    try:
        os.kill(pid, 0)
    except OSError as e:
        print(f"Process {pid} not accessible: {e}", file=sys.stderr)
        sys.exit(1)
    
    tracker = ProcessTracker(pid, args.output, args.interval)
    tracker.track()


if __name__ == "__main__":
    main()