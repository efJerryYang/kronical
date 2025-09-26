#!/usr/bin/env python3
"""Analyze Kronical system-tracker metrics with percentile summaries."""

import argparse
import datetime as dt
import math
import sys
from collections import defaultdict
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Sequence, Tuple

try:
    import sqlite3  # type: ignore
except ImportError as exc:  # pragma: no cover
    print(f"sqlite3 module unavailable: {exc}", file=sys.stderr)
    sys.exit(1)

try:
    import duckdb  # type: ignore
except ImportError:  # pragma: no cover
    duckdb = None  # type: ignore

Row = Tuple[dt.datetime, int, float, int, int]


def detect_backend(db_path: Path) -> str:
    ext = db_path.suffix.lower()
    if ext in {".duckdb", ".ddb"}:
        return "duckdb"
    if ext in {".sqlite", ".sqlite3", ".db"}:
        return "sqlite"
    # default: try sqlite first, fall back to duckdb if needed
    if duckdb is None:
        return "sqlite"
    try:
        conn = sqlite3.connect(db_path)
        conn.close()
        return "sqlite"
    except sqlite3.Error:
        return "duckdb"


def build_filters(pid: Optional[int], start: Optional[str], end: Optional[str]) -> Tuple[str, List]:
    clauses: List[str] = []
    params: List = []
    if pid is not None:
        clauses.append("pid = ?")
        params.append(pid)
    if start is not None:
        clauses.append("timestamp >= ?")
        params.append(start)
    if end is not None:
        clauses.append("timestamp <= ?")
        params.append(end)
    where = f"WHERE {' AND '.join(clauses)}" if clauses else ""
    return where, params


def fetch_sqlite(db: Path, pid: Optional[int], start: Optional[str], end: Optional[str]) -> Iterable[Row]:
    conn = sqlite3.connect(db)
    conn.row_factory = lambda cursor, row: row
    where, params = build_filters(pid, start, end)
    sql = (
        "SELECT timestamp, pid, cpu_percent, memory_bytes, disk_io_bytes "
        f"FROM system_metrics {where} ORDER BY timestamp ASC"
    )
    cur = conn.execute(sql, params)
    for ts_s, pid_val, cpu, mem, disk in cur:
        yield (
            parse_timestamp(ts_s),
            int(pid_val),
            float(cpu),
            int(mem),
            int(disk),
        )
    conn.close()


def fetch_duckdb(db: Path, pid: Optional[int], start: Optional[str], end: Optional[str]) -> Iterable[Row]:
    if duckdb is None:
        raise RuntimeError("duckdb module is not installed; install 'duckdb' to read DuckDB stores")
    conn = duckdb.connect(str(db))
    where, params = build_filters(pid, start, end)
    sql = (
        "SELECT timestamp, pid, cpu_percent, memory_bytes, disk_io_bytes "
        f"FROM system_metrics {where} ORDER BY timestamp ASC"
    )
    cur = conn.execute(sql, params)
    for ts, pid_val, cpu, mem, disk in cur.fetchall():
        ts_dt = ts if isinstance(ts, dt.datetime) else parse_timestamp(str(ts))
        yield (
            ts_dt,
            int(pid_val),
            float(cpu),
            int(mem),
            int(disk),
        )
    conn.close()


def parse_timestamp(value: str) -> dt.datetime:
    value = value.replace("Z", "+00:00")
    return dt.datetime.fromisoformat(value).astimezone(dt.timezone.utc)


def percentile(data: Sequence[int], pct: float) -> float:
    if not data:
        return float("nan")
    if len(data) == 1:
        return float(data[0])
    k = (len(data) - 1) * pct / 100.0
    f = math.floor(k)
    c = math.ceil(k)
    if f == c:
        return float(data[int(k)])
    d0 = data[f] * (c - k)
    d1 = data[c] * (k - f)
    return float(d0 + d1)


def summarize(values: List[int]) -> Dict[str, float]:
    if not values:
        return {}
    sorted_vals = sorted(values)
    return {
        "min": float(sorted_vals[0]),
        "p50": percentile(sorted_vals, 50),
        "p90": percentile(sorted_vals, 90),
        "p95": percentile(sorted_vals, 95),
        "p99": percentile(sorted_vals, 99),
        "max": float(sorted_vals[-1]),
    }


def human_bytes(value: float) -> str:
    units = ["B", "KB", "MB", "GB", "TB", "PB"]
    n = float(value)
    for unit in units:
        if abs(n) < 1024.0:
            return f"{n:,.2f} {unit}"
        n /= 1024.0
    return f"{n:,.2f} EB"


def format_stats(label: str, stats: Dict[str, float]) -> str:
    if not stats:
        return f"{label:<10} (no data)"
    return (
        f"{label:<10} min={human_bytes(stats['min'])} "
        f"p50={human_bytes(stats['p50'])} p90={human_bytes(stats['p90'])} "
        f"p95={human_bytes(stats['p95'])} p99={human_bytes(stats['p99'])} "
        f"max={human_bytes(stats['max'])}"
    )


def aggregate(rows: Iterable[Row]) -> Tuple[int, Dict[str, float], Dict[str, float], Dict[dt.date, Dict[str, Dict[str, float]]]]:
    count = 0
    mem_values: List[int] = []
    disk_values: List[int] = []
    daily_mem: Dict[dt.date, List[int]] = defaultdict(list)
    daily_disk: Dict[dt.date, List[int]] = defaultdict(list)

    for ts, _pid, _cpu, mem, disk in rows:
        count += 1
        mem_values.append(mem)
        disk_values.append(disk)
        day = ts.date()
        daily_mem[day].append(mem)
        daily_disk[day].append(disk)

    overall_mem = summarize(mem_values)
    overall_disk = summarize(disk_values)
    timeline: Dict[dt.date, Dict[str, Dict[str, float]]] = {}
    for day in sorted(daily_mem.keys() | daily_disk.keys()):
        timeline[day] = {
            "memory": summarize(daily_mem.get(day, [])),
            "disk": summarize(daily_disk.get(day, [])),
        }
    return count, overall_mem, overall_disk, timeline


def print_report(
    total_rows: int,
    overall_mem: Dict[str, float],
    overall_disk: Dict[str, float],
    timeline: Dict[dt.date, Dict[str, Dict[str, float]]],
) -> None:
    print(f"Loaded {total_rows} samples")
    print("\nOverall memory usage:")
    print("  " + format_stats("memory", overall_mem))
    print("\nOverall disk delta per sample:")
    print("  " + format_stats("disk", overall_disk))

    if not timeline:
        return

    print("\nDaily summary (UTC):")
    header = (
        f"{'date':<12}"
        f"{'mem p50':>14}{'mem p95':>14}{'mem max':>14}"
        f"{'disk p50':>14}{'disk p95':>14}{'disk max':>14}"
    )
    print(header)
    print("-" * len(header))
    for day, stats in timeline.items():
        mem = stats.get("memory", {})
        disk = stats.get("disk", {})
        def fmt(stat_key: str, data: Dict[str, float]) -> str:
            return human_bytes(data.get(stat_key, float("nan"))) if data else "-"

        print(
            f"{day.isoformat():<12}"
            f"{fmt('p50', mem):>14}{fmt('p95', mem):>14}{fmt('max', mem):>14}"
            f"{fmt('p50', disk):>14}{fmt('p95', disk):>14}{fmt('max', disk):>14}"
        )


def main() -> int:
    parser = argparse.ArgumentParser(description="Summarize system tracker metrics")
    parser.add_argument("--db", required=True, help="Path to tracker database (SQLite or DuckDB)")
    parser.add_argument("--pid", type=int, help="Filter by PID")
    parser.add_argument(
        "--start",
        help="ISO-8601 start timestamp (inclusive, e.g. 2024-05-01T00:00:00Z)",
    )
    parser.add_argument(
        "--end",
        help="ISO-8601 end timestamp (inclusive, e.g. 2024-05-07T23:59:59Z)",
    )
    args = parser.parse_args()

    db_path = Path(args.db).expanduser()
    if not db_path.exists():
        print(f"database not found: {db_path}", file=sys.stderr)
        return 1

    backend = detect_backend(db_path)
    if backend == "sqlite":
        rows = list(fetch_sqlite(db_path, args.pid, args.start, args.end))
    else:
        rows = list(fetch_duckdb(db_path, args.pid, args.start, args.end))

    if not rows:
        print("No samples matched the query")
        return 0

    total, mem_stats, disk_stats, timeline = aggregate(rows)
    print_report(total, mem_stats, disk_stats, timeline)
    return 0


if __name__ == "__main__":
    sys.exit(main())
