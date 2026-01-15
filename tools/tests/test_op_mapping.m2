try (
    -- Let's define the safe name mapping function to be used in extraction
    -- We'll put it in a separate file to test it first
    
    opMapping = new HashTable from {
        "*" => "star",
        "+" => "plus",
        "-" => "minus",
        "/" => "slash",
        "%" => "percent",
        "^" => "caret",
        "!" => "bang",
        "?" => "question",
        "=" => "eq",
        "<" => "lt",
        ">" => "gt",
        "|" => "bar",
        "&" => "amp",
        "@" => "at",
        "#" => "hash",
        "$" => "dollar",
        "~" => "tilde",
        "\\" => "bs",
        ":" => "colon",
        ";" => "semicolon",
        "," => "comma",
        "." => "dot",
        "_" => "underscore"
    }

    getSafeName = (op) -> (
        s = toString op;
        if s === "" then return "empty";
        
        -- If strictly alphanumeric, return as is (but sanitize just in case)
        isAlpha = match("^[a-zA-Z0-9]+$", s);
        if isAlpha then return s;
        
        -- If map has exact match
        if opMapping#?s then return "_" | opMapping#s;
        
        -- Otherwise character by character
        res = "";
        scan(s, c -> (
            c = toString c;
            if opMapping#?c then res = res | "_" | opMapping#c
            else if match("^[a-zA-Z0-9]$", c) then res = res | c
            else res = res | "_x" | (toString toASCII c); -- fallback
        ));
        res
    )
    
    print ("+ -> " | getSafeName(+))
    print ("** -> " | getSafeName(**))
    print ("==> -> " | getSafeName(==>))
    print ("_ -> " | getSafeName(_))
    print ("\\ -> " | getSafeName(\))
    
) else print "Failed"
exit 0
