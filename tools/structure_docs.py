from __future__ import annotations
from typing import Optional, List, Dict, Set, Any, Union

class CodeResult:
    def __init__(self, result: str):
        self.string = result

class Instance:
    def __init__(self, name: str, safe_name: str, type_: Optional[Type], description: str, examples: List[str], additional_info: str, extra: Dict[str, Any]):
        self.name = name
        self.safe_name = safe_name
        self.type = type_
        if self.type is not None:
            self.type.instances.append(self)
        self.description = description
        self.examples = examples
        self.additional_info = additional_info
        self.extra = extra

    def __hash__(self):
        return hash((self.name))
    
    def __repr__(self):
        return f"<{self.__class__.__name__} {self.name}>"

class Type(Instance):
    def __init__(self, name: str, safe_name: str, type_: Optional[Type], description: str, examples: List[str], additional_info: str,
                parent: Optional[Type], extra: Dict[str, Any]):
        super().__init__(name, safe_name, type_, description, examples, additional_info, extra)
        self.parent = parent
        if self.parent is not None:
            self.parent.subtypes.add(self)
        self.subtypes: Set[Type] = set()
        self.instances: List[Instance] = []
        self.return_type_of: List[Union[Function, Installation]] = []
        self.arg_type_of: List[Union[Function, Installation]] = []

class Function(Instance):
    def __init__(self, name: str, safe_name: str, type_: Optional[Type], description: str, examples: List[str], additional_info: str,
                return_type: Optional[Type], arg_type: Optional[Type], 
                number_of_vars: Optional[int], options: Optional[Dict], extra: Dict[str, Any]):
        super().__init__(name, safe_name, type_, description, examples, additional_info, extra)
        self.return_type = return_type
        if self.return_type is not None:
            self.return_type.return_type_of.append(self)
        self.arg_type = arg_type
        if self.arg_type is not None:
            self.arg_type.arg_type_of.append(self)
        self.number_of_vars = number_of_vars
        self.options = options

class Method(Function):
    def __init__(self, name: str, safe_name: str, type_: Optional[Type], description: str, examples: List[str], additional_info: str,
                return_type: Optional[Type], arg_type: Optional[Type], 
                number_of_vars: Optional[int], options: Optional[Dict], extra: Dict[str, Any]):
        super().__init__(name, safe_name, type_, description, examples, additional_info, return_type, arg_type, number_of_vars, options, extra)
        self.installations: List[Installation] = []

class Installation:
    def __init__(self, method: Method, domain: List[Type], codomain: Optional[Type], 
    description: Optional[str], examples: List[str], options: Optional[Dict], extra: Dict[str, Any]):
        self.method = method
        method.installations.append(self)
        self.domain = domain
        for type_ in domain:
            type_.arg_type_of.append(self)
        self.codomain = codomain
        if codomain is not None:
            codomain.return_type_of.append(self)
        self.description = description
        self.examples = examples
        self.options = options
        self.extra = extra

    def __hash__(self):
        return hash((self.method, tuple(self.domain), self.codomain))
    
    def __repr__(self):
        dom_str = ", ".join(t.name for t in self.domain)
        return f"<Installation {self.method.name}({dom_str})>"

class Option(Instance):
    def __init__(self, name: str, safe_name: str, description: str, value_type: Optional[str], extra: Dict[str, Any]):
        super().__init__(name, safe_name, None, description, [], "", extra)
        self.value_type = value_type
