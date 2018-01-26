#!/usr/bin/env python3

# This script makes the following assumptions:
#  * miniconda is installed remotely at ~/miniconda
#  * misoc and artiq are installed remotely via conda

import sys
import argparse
import logging
import subprocess
import socket
import select
import threading
import os
import shutil
import re

from artiq.tools import verbosity_args, init_logger
from artiq.remoting import SSHClient

logger = logging.getLogger(__name__)


def get_argparser():
    parser = argparse.ArgumentParser(
        description="ARTIQ core device development tool",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter)

    verbosity_args(parser)

    parser.add_argument("-t", "--target", metavar="TARGET",
                        type=str, default="kc705",
                        help="target to build, one of: "
                             "kc705 kasli sayma_rtm sayma_amc")
    parser.add_argument("-g", "--build-gateware",
                        default=False, action="store_true",
                        help="build gateware, not just software")
    parser.add_argument("-H", "--host",
                        type=str, default="lab.m-labs.hk",
                        help="SSH host where the development board is located")
    parser.add_argument("-b", "--board",
                        type=str, default="{board_type}-1",
                        help="board to connect to on the development SSH host")
    parser.add_argument("-B", "--board-file",
                        type=str, default="/var/lib/artiq/boards/{board}",
                        help="the board file containing the openocd initialization commands; "
                             "it is also used as the lock file")
    parser.add_argument("-s", "--serial",
                        type=str, default="/dev/ttyUSB_{board}",
                        help="TTY device corresponding to the development board")
    parser.add_argument("-d", "--device",
                        type=str, default="{board}.{host}",
                        help="address or domain corresponding to the development board")
    parser.add_argument("-w", "--wait", action="store_true",
                        help="wait for the board to unlock instead of aborting the actions")

    parser.add_argument("actions", metavar="ACTION",
                        type=str, default=[], nargs="+",
                        help="actions to perform, sequence of: "
                             "build clean reset flash flash+log connect hotswap")

    return parser


def main():
    args = get_argparser().parse_args()
    init_logger(args)
    if args.verbose == args.quiet == 0:
        logging.getLogger().setLevel(logging.INFO)

    def build_dir(*path, target=args.target):
        return os.path.join("/tmp", target, *path)

    extra_build_args = []
    if args.target == "kc705":
        board_type, firmware = "kc705", "runtime"
    elif args.target == "sayma_amc":
        board_type, firmware = "sayma_amc", "runtime"
        extra_build_args += ["--rtm-csr-csv", build_dir("sayma_rtm_csr.csv", target="sayma_rtm")]
    elif args.target == "sayma_rtm":
        board_type, firmware = "sayma_rtm", None
    else:
        raise NotImplementedError("unknown target {}".format(args.target))

    board      = args.board.format(board_type=board_type)
    board_file = args.board_file.format(board=board)
    device     = args.device.format(board=board, host=args.host)
    serial     = args.serial.format(board=board)

    client = SSHClient(args.host)

    flock_acquired = False
    flock_file = None # GC root
    def lock():
        nonlocal flock_acquired
        nonlocal flock_file

        if not flock_acquired:
            fuser_args = ["fuser", "-u", board_file]
            fuser = client.spawn_command(fuser_args)
            fuser_file = fuser.makefile('r')
            fuser_match = re.search(r"\((.+?)\)", fuser_file.readline())
            if fuser_match and fuser_match.group(1) == os.getenv("USER"):
                logger.info("Lock already acquired by {}".format(os.getenv("USER")))
                flock_acquired = True
                return

            logger.info("Acquiring device lock")
            flock_args = ["flock"]
            if not args.wait:
                flock_args.append("--nonblock")
            flock_args += ["--verbose", board_file]
            flock_args += ["sleep", "86400"]

            flock = client.spawn_command(flock_args, get_pty=True)
            flock_file = flock.makefile('r')
            while not flock_acquired:
                line = flock_file.readline()
                if not line:
                    break
                logger.debug(line.rstrip())
                if line.startswith("flock: executing"):
                    flock_acquired = True
                elif line.startswith("flock: failed"):
                    logger.error("Failed to get lock")
                    sys.exit(1)

    def command(*args, on_failure="Command failed"):
        try:
            subprocess.check_call(args)
        except subprocess.CalledProcessError:
            logger.error(on_failure)
            sys.exit(1)

    def flash(*steps):
        lock()

        flash_args = ["artiq_flash"]
        for _ in range(args.verbose):
            flash_args.append("-v")
        flash_args += ["-H", args.host, "-t", board_type]
        flash_args += ["--srcbuild", build_dir()]
        flash_args += ["--preinit-command", "source {}".format(board_file)]
        flash_args += steps
        command(*flash_args, on_failure="Flashing failed")

    for action in args.actions:
        if action == "build":
            logger.info("Building target")

            build_args = ["python3", "-m", "artiq.gateware.targets." + args.target]
            if not args.build_gateware:
                build_args.append("--no-compile-gateware")
            build_args += ["--output-dir", build_dir()]
            build_args += extra_build_args
            command(*build_args, on_failure="Build failed")

        elif action == "clean":
            logger.info("Cleaning build directory")
            shutil.rmtree(build_dir(), ignore_errors=True)

        elif action == "reset":
            logger.info("Resetting device")
            flash("start")

        elif action == "flash":
            logger.info("Flashing and booting firmware")
            flash("proxy", "bootloader", "firmware", "start")

        elif action == "flash+log":
            logger.info("Flashing firmware")
            flash("proxy", "bootloader", "firmware")

            flterm = client.spawn_command(["flterm", serial, "--output-only"])
            logger.info("Booting firmware")
            flash("start")
            client.drain(flterm)

        elif action == "connect":
            lock()

            transport = client.get_transport()
            transport.set_keepalive(30)

            def forwarder(local_stream, remote_stream):
                try:
                    while True:
                        r, _, _ = select.select([local_stream, remote_stream], [], [])
                        if local_stream in r:
                            data = local_stream.recv(65535)
                            if data == b"":
                                break
                            remote_stream.sendall(data)
                        if remote_stream in r:
                            data = remote_stream.recv(65535)
                            if data == b"":
                                break
                            local_stream.sendall(data)
                except Exception as err:
                    logger.error("Cannot forward on port %s: %s", port, repr(err))
                local_stream.close()
                remote_stream.close()

            def listener(port):
                listener = socket.socket()
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                listener.bind(('localhost', port))
                listener.listen(8)
                while True:
                    local_stream, peer_addr = listener.accept()
                    logger.info("Accepting %s:%s and opening SSH channel to %s:%s",
                                *peer_addr, device, port)
                    try:
                        remote_stream = \
                            transport.open_channel('direct-tcpip', (device, port), peer_addr)
                    except Exception:
                        logger.exception("Cannot open channel on port %s", port)
                        continue

                    thread = threading.Thread(target=forwarder, args=(local_stream, remote_stream),
                                              name="forward-{}".format(port), daemon=True)
                    thread.start()

            ports = [1380, 1381, 1382, 1383]
            for port in ports:
                thread = threading.Thread(target=listener, args=(port,),
                                          name="listen-{}".format(port), daemon=True)
                thread.start()

            logger.info("Forwarding ports {} to core device and logs from core device"
                            .format(", ".join(map(str, ports))))
            client.run_command(["flterm", serial, "--output-only"])

        elif action == "hotswap":
            logger.info("Hotswapping firmware")
            firmware = build_dir("software", firmware, firmware + ".bin")
            command("artiq_coreboot", "hotswap", firmware,
                    on_failure="Hotswapping failed")

        else:
            logger.error("Unknown action {}".format(action))
            sys.exit(1)

if __name__ == "__main__":
    main()
