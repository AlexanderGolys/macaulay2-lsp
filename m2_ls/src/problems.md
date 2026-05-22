<!--
This file records upstream Macaulay2 bugs or surprising behaviors found while
developing the language server. It is not a bug list for this repository.
-->

```macaulay2
help 
================
initial help -- Welcome to Macaulay2
       ************************************

       Try entering "2+1.5" at your next input prompt, which begins with "i" (e.g " i2 : ").  The two output prompts begin with "o".

         * the first one, for instance "o2 = ", gives the value computed from your input;
         * the second one, for instance "o2 : ", tells what type of thing the value is.

       Type one of these commands to get started reading the documentation:

       copyright                         -- the copyright
       help "Macaulay2"                  -- top node of the documentation.
       help "reading the documentation"
       help "getting started"
       help "a first Macaulay2 session"
       help coker                        -- show documentation for coker
       help about Ext                    -- show documentation about Ext
       help about("Yoneda", Body=>true)  -- show documentation mentioning "Yoneda"
       printWidth = 80                   -- set print width to 80 characters
       viewHelp                          -- view documentation in a browser
       viewHelp coker                    -- view documentation for coker in browser
       ? hilbertFunction                 -- display brief documentation about Hilbert functions
```


```macaulay2
help about Ext
================
stdio:488:4:(3):[7]: error: expected argument 2 to be a function
```
identity(
