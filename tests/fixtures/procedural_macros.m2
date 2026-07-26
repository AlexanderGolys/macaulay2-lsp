needsPackage "ProceduralMacros"

x = $twice 3 + 4 $
y = $outer $inner x $ $

-- Macro-looking text in strings and comments is intentionally inert.
message = "$twice 3 $"
-- $twice 3 $
