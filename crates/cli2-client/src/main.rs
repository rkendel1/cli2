//! `cli2-client-demo`: the end-to-end flow of an embedding application, using
//! only the protocol. It prints human-readable lines for people; programs
//! should use the `Client` library and the typed results instead.

use cli2_client::Client;
use cli2_protocol::EventData;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let (Some(socket), Some(project), Some(intent)) =
        (get("--socket"), get("--project"), get("--intent"))
    else {
        eprintln!("usage: cli2-client-demo --socket PATH --project DIR --intent TEXT [--reject | --approve]");
        std::process::exit(2);
    };
    if let Err(e) = run(
        &socket,
        &project,
        &intent,
        args.iter().any(|a| a == "--reject"),
        args.iter().any(|a| a == "--approve"),
    ) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(
    socket: &str,
    project: &str,
    intent: &str,
    reject: bool,
    approve: bool,
) -> Result<(), cli2_client::ClientError> {
    let mut c = Client::connect(socket, "cli2-client-demo")?;
    println!(
        "connected: protocol {} to {} {}",
        c.server().protocol_version,
        c.server().server.name,
        c.server().instance_id
    );
    let plan = c.plan(project, intent)?;
    println!(
        "status: {:?}; approvability: {:?}",
        plan.outcome.status, plan.approvability
    );
    let Some(p) = plan.outcome.plan.clone() else {
        println!("no plan: {}", plan.outcome.message);
        return Ok(());
    };
    println!("plan {} digest {}", p.plan_id, p.digest);
    println!(
        "would run: {:?} in {}",
        p.steps
            .iter()
            .map(|s| s.display_command())
            .collect::<Vec<_>>(),
        p.steps[0]
            .as_process()
            .map(|s| s.working_directory.as_str())
            .unwrap_or("")
    );
    if reject {
        let r = c.reject(&p.plan_id, Some("rejected by the demo"))?;
        println!("rejected: {:?}", r.outcome.status);
        return Ok(());
    }
    if !approve {
        println!("(not approved; nothing executed)");
        return Ok(());
    }
    let approval = c.approve(&p.plan_id, &p.digest)?.approval;
    let run = c.start(&p.plan_id, &approval.approval_id)?;
    println!("started {}", run.execution_id);
    let done = c.wait(&run.execution_id, |e| match &e.data {
        EventData::Output { stream, text, .. } => print!("[{stream:?}] {text}"),
        EventData::ExecutionFinished { .. } => println!("finished"),
        other => println!("event: {other:?}"),
    })?;
    println!(
        "process: {:?}; task: {:?}; verification: {:?}",
        done.process, done.task_status, done.verification
    );
    Ok(())
}
