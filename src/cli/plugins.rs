use crate::{config::launch, plugins::PluginRegistry};

pub(super) fn run(args: &[String]) -> Result<(), String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("cells plugins check CONFIG [PLUGIN]\nValidate all configured plugins, or one plugin and its dependencies, without starting services or writing stores.");
        return Ok(());
    }
    if !(args.len() == 2 || args.len() == 3) || args[0] != "check" {
        return Err("Use cells plugins check CONFIG [PLUGIN]".into());
    }
    let config = launch::load(std::path::Path::new(&args[1])).map_err(|error| error.to_string())?;
    let registry = PluginRegistry::builtins();
    let plan = match args.get(2) {
        Some(target) => registry.check(&config, target)?,
        None => registry.resolve(&config)?.1,
    };
    println!(
        "{}",
        serde_json::json!({"valid":true,"plugins":plan.statuses(),"order":plan.order()})
    );
    Ok(())
}
