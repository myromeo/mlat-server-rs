# Test-only launcher: run the unmodified mlat-server with Python's cyclic
# garbage collector disabled, to measure how much CPU it accounts for.
import gc, runpy, sys
gc.disable()
sys.path.insert(0, "/opt/mlat-server")  # as if started via `python3 /opt/mlat-server/mlat-server`
sys.argv[0] = "/opt/mlat-server/mlat-server"
runpy.run_path("/opt/mlat-server/mlat-server", run_name="__main__")
