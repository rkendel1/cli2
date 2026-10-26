#!/usr/bin/env python3
"""A minimal cli2 protocol client. Standard library only; no cli2 code imported.

    cli2 serve --socket /tmp/cli2.sock &
    python3 client.py /tmp/cli2.sock /abs/path/to/project "run the project tests" [--reject|--approve]

It plans an intent, shows the plan from the typed fields, and then (only if
told to) rejects it or approves that exact plan, runs it, follows the events
and prints the outcome. It never parses terminal text and has no way to run a
command that was not planned and approved.
"""
import json, socket, sys

class Cli2:
    def __init__(self, path, name="example-client"):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(path)
        self.f = self.sock.makefile("rw", encoding="utf-8", newline="\n")
        self.n = 0
        hello = self.call("hello", {"client": {"name": name}, "versions": [1]})
        assert hello["protocol_version"] == 1, hello

    def call(self, op, params=None):
        self.n += 1
        self.f.write(json.dumps({"id": str(self.n), "op": op, "params": params or {}}) + "\n")
        self.f.flush()
        resp = json.loads(self.f.readline())
        if not resp["ok"]:
            raise RuntimeError(f'{resp["error"]["code"]}: {resp["error"]["message"]}')
        return resp["result"]

def main():
    socket_path, project, intent, *flags = sys.argv[1:]
    c = Cli2(socket_path)
    r = c.call("intent.plan", {"project": project, "intent": intent})
    outcome, plan = r["outcome"], r["outcome"].get("plan")
    print(f'status={outcome["status"]} approvability={r["approvability"]}')
    if not plan:
        print("no plan:", outcome["message"])
        return
    step = plan["steps"][0]
    print("command:", step["argv"] if "argv" in step else [step["executable"], *step["arguments"]])
    print("directory:", step["working_directory"], "| approval:", plan["approval"]["level"], "| plan:", plan["plan_id"])
    if "--reject" in flags:
        print("rejected:", c.call("plan.reject", {"plan_id": plan["plan_id"], "reason": "example"})["outcome"]["status"])
        return
    if "--approve" not in flags:
        print("not approved; nothing executed")
        return
    # Approval names the exact plan by ID and full digest; it authorizes nothing by itself.
    approval = c.call("plan.approve", {"plan_id": plan["plan_id"], "plan_digest": plan["digest"]})["approval"]
    run = c.call("execution.start", {"plan_id": plan["plan_id"], "approval_id": approval["approval_id"]})
    after, finished = 0, False
    while not finished:  # pull events with a cursor; safe to reconnect and resume from `after`
        batch = c.call("execution.events", {"execution_id": run["execution_id"], "after_seq": after, "wait_ms": 2000})
        for e in batch["events"]:
            after = e["seq"]
            if e["type"] == "output":
                print(f'[{e["stream"]}] {e["text"]}', end="")
        finished = batch["finished"]
    done = c.call("execution.get", {"execution_id": run["execution_id"]})
    # Three separate facts: how the process ended, the task status, the verification.
    print(json.dumps({"process": done["process"], "task_status": done["task_status"], "verification": done["verification"]}))

main()
