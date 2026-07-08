import json, sys
path = sys.argv[1]
with open(path, encoding='utf-8') as f:
    for line in f:
        r = json.loads(line)
        print(f"round {r.get('round','?')}: phase={r.get('phase','?')} status={r.get('status','?')}")
        s = r.get('summary','')
        if s:
            print(f"  summary: {s[:200]}")
        for step in r.get('steps', []):
            sid = step.get('step_id','?')
            ok = step.get('success', None)
            ss = step.get('summary','')
            print(f"  step {sid}: success={ok} {ss[:150]}")
