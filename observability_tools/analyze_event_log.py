import json
import sys
from collections import defaultdict

def analyze_log(file_path):
    events = []
    with open(file_path, 'r') as f:
        for line in f:
            if line.strip():
                events.append(json.loads(line))
                
    counts = defaultdict(int)
    artifacts = []
    
    for e in events:
        counts[e.get('event_type')] += 1
        if e.get('event_type') == 'ArtifactCreated':
            artifacts.append(e.get('payload', {}))
            
    print("=== Event Counts ===")
    for k, v in counts.items():
        print(f"{k}: {v}")
        
    print(f"\nTotal Artifacts: {len(artifacts)}")
    
if __name__ == '__main__':
    analyze_log(sys.argv[1])
