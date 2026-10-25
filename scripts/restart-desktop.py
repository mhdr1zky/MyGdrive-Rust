"""Wait for the old desktop process to exit before replacing this helper with its launcher."""
import argparse
import os
from pathlib import Path
import select
import time


def process_start(pid):
    try:
        return Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[19]
    except (OSError, IndexError):
        return None


def wait_for_exit(pid, expected_start, timeout=30):
    if process_start(pid) != expected_start:
        return
    deadline = time.monotonic() + timeout
    if hasattr(os, 'pidfd_open'):
        try:
            descriptor = os.pidfd_open(pid)
        except ProcessLookupError:
            return
        except OSError:
            descriptor = None
        if descriptor is not None:
            try:
                if process_start(pid) != expected_start:
                    return
                if not select.select([descriptor], [], [], timeout)[0]:
                    raise TimeoutError('Old desktop has not exited; automatic relaunch cancelled')
                return
            finally:
                os.close(descriptor)
    while process_start(pid) == expected_start:
        if time.monotonic() >= deadline:
            raise TimeoutError('Old desktop has not exited; automatic relaunch cancelled')
        time.sleep(.05)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--pid', type=int, required=True)
    parser.add_argument('--start-time', required=True)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command or not Path(command[0]).is_absolute():
        raise ValueError('An absolute desktop launcher is required')
    os.setsid()
    wait_for_exit(args.pid, args.start_time)
    os.execv(command[0], command)


if __name__ == '__main__':
    main()
