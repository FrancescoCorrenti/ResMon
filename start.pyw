"""Start ResMon as ONE process: run it with the base pythonw.exe and the venv's packages, instead of the
venv's pythonw.exe, which is only a launcher that stays alive next to the real interpreter.

Shortcut target: <base Python>\\pythonw.exe "<this folder>\\start.pyw"
"""
import os
import runpy
import site
import sys

here = os.path.dirname(os.path.abspath(__file__))
site.addsitedir(os.path.join(here, ".venv", "Lib", "site-packages"))  # also runs pywin32's .pth
os.chdir(here)
sys.path.insert(0, here)
sys.argv = [os.path.join(here, "app.py")]
runpy.run_path(sys.argv[0], run_name="__main__")
