

symbolForName = (runtimeDict, name) -> (
    if runtimeDict#?name then runtimeDict#name
    else if isGlobalSymbol name then getGlobalSymbol name
    else null
)
