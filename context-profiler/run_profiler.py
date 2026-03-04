import sys
import os

# Ensure the 'src' directory is in PYTHONPATH
sys.path.append(os.path.join(os.path.dirname(__file__), 'src'))

from context_profiler.main import main

if __name__ == "__main__":
    main()
