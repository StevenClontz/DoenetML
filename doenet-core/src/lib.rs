pub mod prelude;

pub mod state_variables;
pub mod component;

pub mod state_var;
pub mod parse_json;
pub mod utils;

use lazy_static::lazy_static;
use state_var::StateVar;
use std::collections::HashMap;
use std::fmt::Debug;
use std::{error::Error, fmt::Display};


use crate::prelude::*;
use crate::component::*;

use state_var::{State, EssentialStateVar};
use state_variables::*;


/// This stores the components and dependency graph.
#[derive(Debug)]
pub struct DoenetCore {
    pub component_nodes: HashMap<ComponentName, ComponentNode>,
    pub component_states: HashMap<ComponentName, HashMap<StateVarName, StateVar>>,
    pub root_component_name: ComponentName,

    /// Stores every dependency, indexed by the name of the component and the state variable.
    pub dependencies: HashMap<ComponentName, HashMap<StateVarReference, HashMap<InstructionName, Dependency>>>,

    /// States that the user can change and which state variables may depend on
    /// (ex: the contents of input dialogues)
    pub essential_data: HashMap<String, EssentialStateVar>,

    /// We send components to the renderer that do not exists
    /// - the inherited children of a copy
    ///
    /// The renderer needs to recognize these as a different component so we alias its name.
    /// This maps the name the renderer is given to the actual name.
    pub aliases: HashMap<String, ComponentName>,
}



/// A collection of edges on the dependency tree
#[derive(Debug)]
pub enum Dependency {
    StateVar(StateVarDependency),
    Essential(EssentialDependency),
}



/// This stores some of the state variables (or strings) that a state variable depends on.
/// Note that a Dependency struct contains multiple edges of the dependency tree.
#[derive(Debug)]
pub struct StateVarDependency {

    // We will use outer product of entries (except for the strings, which don't have state vars)
    pub depends_on_objects: Vec<ObjectName>,
    pub depends_on_state_vars: Vec<StateVarName>,

    pub variables_optional: bool,
}


/// This stores the name of an essential datum that a state variable depends on.
#[derive(Debug)]
pub struct EssentialDependency {
    pub depends_on_essential: String,
}



const SHADOW_INSTRUCTION_NAME: &'static str = "shadow_instruction";




/// This error type should be used for any error that is caused by the 
/// user inputting invalid doenetML. It should only be thrown on core creation.
#[derive(Debug)]
pub enum DoenetMLError {
    ComponentDoesNotExist {
        comp_name: String
    },
    StateVarDoesNotExist {
        comp_name: String,
        sv_name: String,
    },
    AttributeDoesNotExist {
        comp_name: String,
        attr_name: String,
    },
    InvalidComponentType {
        comp_type: String,
    },

    ComponentCopiesAncestor {
        comp_name: String,
        ancestor_name: String,
    }
}

impl Error for DoenetMLError {}
impl Display for DoenetMLError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use DoenetMLError::*;

        match self {
            ComponentDoesNotExist { comp_name } => 
                write!(f, "Component {} does not exist", comp_name),
            StateVarDoesNotExist { comp_name, sv_name } =>
                write!(f, "State variable {} does not exist on {}", sv_name, comp_name),
            AttributeDoesNotExist { comp_name, attr_name } =>
                write!(f, "Attribute {} does not exist on {}", attr_name, comp_name),
            InvalidComponentType { comp_type } => 
                write!(f, "Component type {} does not exist", comp_type),
            ComponentCopiesAncestor { comp_name, ancestor_name } => 
                write!(f, "Component {} copies ancestor {}", comp_name, ancestor_name),
        }
    }
}
// impl From<&str> for DoenetMLError {
//     fn from(message: &str) -> Self {
//         DoenetMLError { message: message.to_string()  }
//     }
// }
// impl From<String> for DoenetMLError {
//     fn from(message: String) -> Self {
//         DoenetMLError { message }
//     }
// }




pub fn create_doenet_core(program: &str) -> (DoenetCore, Vec<DoenetMLError>) {

    // Create component nodes.
    let (mut component_nodes, root_component_name, mut doenet_ml_errors) = 
        parse_json::create_components_tree_from_json(program);

    
    // Parse macros and generate components from them
    replace_macros_with_copies(&mut component_nodes);

    let component_nodes = component_nodes;


    // For every component copy, add aliases for children it inherits from its source.
    let mut aliases: HashMap<String, ComponentName> = HashMap::new();
    let copies = component_nodes.iter().filter_map(|(_, c)| match c.copy_source {
        Some(CopySource::Component(ref source)) => Some((c, source)),
        _ => None,
    });

    let mut invalid_copies: Vec<String> = vec![];

    for (copy, source_name) in copies {

        let mut ancestors: Vec<String> = vec![];
        let mut possible_parent = &copy.parent;
        while let Some(parent) = possible_parent {
            ancestors.push(parent.clone());
            possible_parent = &component_nodes.get(parent).unwrap().parent;
        }


        if !component_nodes.contains_key(source_name) {

            // The component tried to copy a non-existent component, so log an error,
            // and pretend it didn't copy anything

            doenet_ml_errors.push(DoenetMLError::ComponentDoesNotExist {
                comp_name: source_name.to_string()
            });

            invalid_copies.push(copy.name.to_string());

            
        } else if ancestors.contains(source_name) {
            // The component tried to copy its ancestor, so log an error,
            // and pretend it doesn't copy anything

            doenet_ml_errors.push(DoenetMLError::ComponentCopiesAncestor {
                comp_name: copy.name.clone(), ancestor_name: source_name.clone()
            });

            invalid_copies.push(copy.name.to_string());


        } else {

            add_alias_for_children(&mut aliases, copy, &component_nodes, &copy.name);

        }
    }

    let mut component_nodes = component_nodes;

    // Remove invalid copy references
    for invalid_copy in invalid_copies {
        let culprit_component = component_nodes.get_mut(&invalid_copy).unwrap();
        culprit_component.copy_source = None;
    }

    let component_nodes = component_nodes;


    // Fill in HashMaps: component_states and dependencies for every component
    // and supply essential_data required by any `EssentialDependency`.

    let mut component_states: HashMap<ComponentName, HashMap<StateVarName, StateVar>> = HashMap::new();
    let mut dependencies:
        HashMap<ComponentName, HashMap<StateVarReference, HashMap<InstructionName, Dependency>>> = HashMap::new();
    let mut essential_data: HashMap<String, EssentialStateVar> = HashMap::new();

    for (component_name, component_node) in component_nodes.iter() {

        let dependencies_for_this_component = create_all_dependencies_for_component(
            &component_nodes,
            component_node,
            &mut essential_data
        );

        dependencies.insert(component_name.to_string(), dependencies_for_this_component);


        let component_state: HashMap<StateVarName, StateVar> = component_node
            .definition
            .state_var_definitions()
            .iter()
            .map(|(&sv_name, sv_variant)| (sv_name, StateVar::new(sv_variant)))
            .collect();


        component_states.insert(
            component_name.clone(),
            component_state,
        );
    }


    log_json!("Components upon core creation",
        utils::json_dependencies(&dependencies));
    log_json!("Dependencies upon core creation",
        utils::json_components(&component_nodes, &component_states));


    (DoenetCore {
        component_nodes,
        component_states,
        root_component_name,
        dependencies,
        essential_data,
        aliases,
    }, doenet_ml_errors)
}


fn name_child_of_copy(child: &str, copy: &str) -> ComponentName {
    format!("__cp:{}({})", child, copy)
}

fn add_alias_for_children(
    aliases: &mut HashMap<String, ComponentName>,
    component: &ComponentNode,
    component_nodes: &HashMap<ComponentName, ComponentNode>,
    copy: &String,
) {
    for (child, _) in get_children_including_copy(component_nodes, component).iter() {
        if let ComponentChild::Component(child_comp) = child {
            aliases.insert(name_child_of_copy(child_comp, copy), child_comp.to_string());
        }
    }
}


fn replace_macros_with_copies(components: &mut HashMap<ComponentName, ComponentNode>) {

    use regex::Regex;
    use std::iter::repeat;


    // One or two $ followed by either
    //   - the following combination without parenthesis:
    //     - a word (starting with a letter or underscore), capturing word as fourth group, 
    //     - optionally followed by anything in square brackets (6th group)
    //     - optionally followed by:
    //       - a period
    //       - followed by a word (9th group)
    //       - optionally followed by anything in square brackets (11th group)
    //   or
    //   - the following combination in parenthesis
    //     where the closing parenthesis could be replaced by an open brace,
    //     (capturing the open brace or closing parens as 21st group):
    //     - an identifier (containing word characters, slash, hyphen, or "../") (12th group)
    //     - optionally followed by anything in square brackets (15th group)
    //     - optionally followed by:
    //       - a period
    //       - followed by a word (including hyphens) (18th group)
    //       - optionally followed by anything in square brackets (20th group)

    lazy_static! {

        // NOTE: It took around ~100ms on a fast computer to create this regex (not including searching with it)
        static ref MACRO_SEARCH: Regex = Regex::new(r"(?x) #flag that ignores whitespace and comments
  
        (\$) # for now, just one $
        (
            (([a-zA-Z_]\w*)(\[([^\[^\]]+)\])?((\.([a-zA-Z]\w*))(\[([^\[^\]]+)\])?)?)
            |
            \(
                (([\w/-]|\.\./)+)(\[([^\[^\]]+)\])?((\.([\w\-]+))(\[([^\[^\]]+)\])?)?\s*
            ( \) | \{ )
        )
    
        ").unwrap();
    
    }

    lazy_static! {
        static ref POSSIBLE_MACRO: Regex = Regex::new("$").unwrap();
    }



    // Keyed by the component name and by the original position of the child we are replacing
    let mut replacement_children: HashMap<ComponentName, HashMap<usize, Vec<ObjectName>>> = HashMap::new();

    let mut components_to_add: Vec<ComponentNode> = vec![];

    // Keyed on the source component names, or the source (component name, state var)
    let mut macro_copy_counter: HashMap<ComponentName, u32> = HashMap::new();
    

    // This iterator gives info for every string child
    // (original index of child, string value, component)
    let all_string_children = components.iter()
        .flat_map(| (_, comp) |
            comp.children.iter()
                .enumerate()
                .filter_map(| (id, child) | {
                    match child {
                        ObjectName::String(string_val) => Some((id, string_val)),
                        _ => None,
                    }
                })
                .zip(repeat(comp))
                .map(|((id, val), comp)| (id, val, comp))
            );


    for (child_id, string_val, component) in all_string_children {
        let mut new_children = vec![];
        let mut previous_end = 0;

        if ! POSSIBLE_MACRO.is_match(string_val) {
            continue;
        }


        for capture in MACRO_SEARCH.captures_iter(string_val) {

            log_debug!("capture {:#?}", capture);

            let start = capture.get(0).unwrap().start();
            let end = capture.get(0).unwrap().end();

            if start == 0 || string_val.chars().nth(start-1).unwrap_or_default() != '$' {

                // Append the regular string from last endpoint up until start of macro
                let before = &string_val[previous_end..start];
                if !before.trim().is_empty() {
                    new_children.push(ComponentChild::String(before.to_string()));
                }



                let (macro_comp, macro_prop_option) = if let Some(comp) = capture.get(12) {
                    (comp, capture.get(18))
                } else {
                    (capture.get(4).unwrap(), capture.get(9))
                };

                if let Some(ending_delim) = capture.get(21) {
                    if ending_delim.as_str() == "{" {
                        panic!("Haven't implemented macros with curly braces");
                    }
                }

                let source_name = macro_comp.as_str();


                let source_comp = components.get(source_name).expect(
                    &format!("Macro for {}, but this component does not exist", source_name)
                );


                let macro_copy: ComponentNode = if let Some(macro_prop) = macro_prop_option {

                    let (prop_name, _) = source_comp.definition.state_var_definitions()
                        .get_key_value(macro_prop.as_str())
                        .expect(&format!("Macro asks for {} property, which does not exist", macro_prop.as_str()));

                    let source_comp_sv_name = format!("{}:{}", source_name, prop_name);

                    let copy_comp_type = default_component_type_for_state_var(source_comp, prop_name);

                    let copy_def = component::generate_component_definitions().get(copy_comp_type).unwrap().clone();

                    let copy_num = macro_copy_counter.entry(source_comp_sv_name.clone()).or_insert(0);
                    *copy_num += 1;

                    let copy_name = format!("__mcr:{}({})_{}", source_comp_sv_name, component.name, copy_num);

                    ComponentNode {
                        name: copy_name,
                        parent: Some(component.name.clone()),
                        children: vec![],
    
                        copy_source: Some(CopySource::StateVar(
                            source_comp.name.clone(), prop_name)),

                        attributes: HashMap::new(),
                        component_type: copy_comp_type,
                        definition: copy_def,
                    }


                } else {

                    let copy_num = macro_copy_counter.entry(source_name.to_string()).or_insert(0);
                    *copy_num += 1;
    
                    let copy_name = format!("__mcr:{}({})_{}", source_name, component.name, copy_num);
    
                    ComponentNode {
                        name: copy_name,
                        parent: Some(component.name.clone()),
                        children: vec![],
    
                        copy_source: Some(CopySource::Component(source_comp.name.clone())),
                        attributes: HashMap::new(),
    
                        .. source_comp.clone()
                    }
                };


                new_children.push(ComponentChild::Component(macro_copy.name.clone()));
                components_to_add.push(macro_copy);


                previous_end = end;

            }

        }

        if previous_end != 0 {

            // There was at least one macro

            let last = &string_val[previous_end..];
            if !last.trim().is_empty() {
                new_children.push(ComponentChild::String(last.to_string()));
            }


            replacement_children.entry(component.name.clone()).or_insert(HashMap::new())
                .entry(child_id).or_insert(new_children);

        }
    }


    log_debug!("Components to add from macros: {:#?}", components_to_add);

    for new_component in components_to_add {

        debug_assert!( !components.contains_key(&new_component.name));
        components.insert(new_component.name.clone(), new_component);
    }


    log_debug!("Replacement children {:#?}", replacement_children);

    for (component_name, new_children_hashmap) in replacement_children {
        
        let component = components.get_mut(&component_name).unwrap();

        for (original_child_id, new_children) in new_children_hashmap {

            // Remove the original element, and add the new children (in order) in its place
            component.children.splice(
                original_child_id..original_child_id + 1,
                new_children
            );

        }
    }

}


fn default_component_type_for_state_var(component: &ComponentNode, state_var: StateVarName) -> ComponentType {

    let state_var_def = component.definition.state_var_definitions().get(state_var).unwrap();
    match state_var_def {
        StateVarVariant::Boolean(_) => "boolean",
        StateVarVariant::Integer(_) => "number",
        StateVarVariant::Number(_) => "number",
        StateVarVariant::String(_) => "text",
        StateVarVariant::NumberArray(_) => panic!(),
    }
}




fn create_all_dependencies_for_component(
    components: &HashMap<ComponentName, ComponentNode>,
    component: &ComponentNode,
    essential_data: &mut HashMap<String, EssentialStateVar>,
) -> HashMap<StateVarReference, HashMap<InstructionName, Dependency>> {

    log_debug!("Creating dependencies for {:?}", component.name);
    let mut dependencies: HashMap<StateVarReference, HashMap<InstructionName, Dependency>> = HashMap::new();


    let my_definitions = component.definition.state_var_definitions();
    for (&state_var_name, &state_var_variant) in my_definitions {

        // All the state vars have keys on the dependency hashmap
        dependencies.entry(StateVarReference::Basic(state_var_name)).or_insert(HashMap::new());

        let dependency_instructions = return_dependency_instruction_including_shadowing(component, state_var_name);


        for (dep_name, ref dep_instruction) in dependency_instructions.into_iter() {

            let dependency = create_dependency_from_instruction(
                &components, component, &StateVarReference::Basic(state_var_name),
                dep_instruction, dep_name, essential_data
            );

            dependencies.entry(StateVarReference::Basic(state_var_name)).or_insert(HashMap::new()).entry(dep_name).or_insert(dependency);
        }

        
        // For array state variables, add the size to the dependency graph
        if state_var_variant.is_array() {
            let size_instructions = state_var_variant.return_size_dependency_instructions(HashMap::new());

            for (dep_name, ref size_instruction) in dependency_instructions.into_iter() {

                let size_dependency = create_dependency_from_instruction(
                    &components, component, &StateVarReference::SizeOf(state_var_name),
                    size_instruction, dep_name, essential_data
                );

                dependencies.entry(StateVarReference::SizeOf(state_var_name)).or_insert(HashMap::new()).entry(dep_name).or_insert(size_dependency);
            }
        }
    }

    dependencies


    // component.definition.state_var_definitions()
    //     .iter()
    //     .flat_map(|(&sv_name, sv_def)|
    //         sv_def.return_dependency_instructions(HashMap::new())
    //             .iter()
    //             .map(|(dep_name, dep_instruction)|
    //                 create_dependency_from_instruction(&components, component, sv_name, dep_instruction, dep_name)
    //             )
    //             .collect::<Vec<_>>()
    //     )
    //     .collect()

}

/// Get the specified if it exists on this component, or on the component it copies
/// Recursively searches the source (and the source's source if it has one), and finds
/// the nearest attribute to the original node, if any exist
fn get_attribute_including_copy<'a>(
    components: &'a HashMap<ComponentName, ComponentNode>,
    component: &'a ComponentNode,
    attribute_name: AttributeName
)-> Option<&'a Attribute> {

    // let source = components.get(source_name).unwrap();
    if let Some(attribute) = component.attributes.get(attribute_name) {
        Some(attribute)

    } else if let Some(CopySource::Component(ref source_name)) = component.copy_source {

        // The hide attribute is an exception: we don't inherit it
        if attribute_name == "hide" {
            None

            
        } else {
            let source = components.get(source_name).unwrap();
            get_attribute_including_copy(components, source, attribute_name)
        }



    } else {
        None
    }
}


/// This function also creates essential data when a DependencyInstruction asks for it.
fn create_dependency_from_instruction(
    components: &HashMap<ComponentName, ComponentNode>,
    component: &ComponentNode,
    state_var_reference: &StateVarReference,
    instruction: &DependencyInstruction,
    instruction_name: InstructionName,
    essential_data: &mut HashMap<String, EssentialStateVar>,
) -> Dependency {

    log_debug!("Creating dependency {}:{:?}:{}", component.name, state_var_reference, instruction_name);

    if let DependencyInstruction::Essential(_) = instruction {
        // An Essential DependencyInstruction returns an Essential Dependency.

        let depends_on_essential = get_key_to_essential_data(components, component, state_var_reference);
        let variant = component.definition.state_var_definitions().get(state_var_reference.name()).unwrap();

        // A copy uses the same essential data, so `insert` would be called twice
        // with the same key, but that's ok.
        essential_data.insert(
            depends_on_essential.clone(),
            EssentialStateVar::new(variant.initial_essential_value()),
        );

        Dependency::Essential( EssentialDependency {
            depends_on_essential,
        })

    } else {
        // Other variants of DependencyInstruction return a StateVar Dependency.

        let depends_on_objects: Vec<ObjectName>;
        let depends_on_state_vars: Vec<StateVarName>;

        match &instruction {

            DependencyInstruction::Essential(_) => unreachable!(),

            DependencyInstruction::StateVar(state_var_instruction) => {

                depends_on_objects = if let Option::Some(ref name) = state_var_instruction.component_name {
                        vec![ObjectName::Component(name.to_string())]
                    } else {
                        vec![ObjectName::Component(component.name.clone())]
                    };
                depends_on_state_vars = vec![state_var_instruction.state_var];
            },

            DependencyInstruction::Child(child_instruction) => {

                let mut depends_on_children: Vec<ObjectName> = vec![];
                for (child, _) in get_children_including_copy(components, component).iter() {

                    match child {
                        ComponentChild::Component(child_component_name) => {
                            let child_component = components.get(child_component_name).unwrap();

                            for desired_child_type in child_instruction.desired_children.iter() {
                                if child_component.definition.get_trait_names().contains(desired_child_type) {

                                    let object_name = ObjectName::Component(child_component.name.clone());

                                    // Add if not already added from previous desired_child_type.
                                    if !depends_on_children.contains(&object_name) {
                                        depends_on_children.push(object_name);
                                    }
                                }
                            }
                        },

                        ComponentChild::String(string_value) => {
                            if child_instruction.desired_children.contains(&ObjectTraitName::TextLike)
                            || child_instruction.desired_children.contains(&ObjectTraitName::NumberLike) {

                                depends_on_children.push(ObjectName::String(string_value.to_owned()));

                            }
                        },

                    }
                }

                depends_on_objects = depends_on_children;
                depends_on_state_vars = child_instruction.desired_state_vars.clone();

            },
            DependencyInstruction::Parent(parent_instruction) => {

                let parent_name = component.parent.clone().expect(&format!(
                    "Component {} doesn't have a parent, but the dependency instruction {:?}:{} asks for one.",
                        component.name, state_var_reference, instruction_name
                ));

                depends_on_objects = vec![ObjectName::Component(parent_name)];
                depends_on_state_vars = vec![parent_instruction.state_var];
            },


            DependencyInstruction::Attribute(attribute_instruction) => {

                let attribute_name = attribute_instruction.attribute_name;

                let possible_attribute: Option<&Attribute> = get_attribute_including_copy(components, component, attribute_name);


                if let Some(attribute) = possible_attribute {
                    match attribute {
                        Attribute::Component(attr_comp_name) => {
                            depends_on_objects = vec![ObjectName::Component(attr_comp_name.to_string())];

                            // hard code this for now
                            depends_on_state_vars = vec!["value"];
                        },

                        Attribute::Primitive(attr_primitive_value) => {
                            depends_on_objects = vec![ObjectName::String(

                                // for now, convert it to a string
                                match attr_primitive_value {
                                    StateVarValue::String(v) => v.to_string(),
                                    StateVarValue::Boolean(v) => v.to_string(),
                                    StateVarValue::Number(v) => v.to_string(),
                                    StateVarValue::Integer(v) => v.to_string(),
                                }
                            )];

                            depends_on_state_vars = vec![];
                        }
                    }

                } else {
                    // Attribute doesn't exist
                    depends_on_objects = vec![];
                    depends_on_state_vars = vec![];
                }

            }

        };

        Dependency::StateVar( StateVarDependency {
            variables_optional: false,

            depends_on_objects,
            depends_on_state_vars,
        })
    }
}


/// A key to the essential_data HashMap is a combination of the component and state var name.
fn get_key_to_essential_data(
    components: &HashMap<ComponentName, ComponentNode>,
    component: &ComponentNode,
    state_var_reference: &StateVarReference,
) -> String {
    match state_var_reference {
        StateVarReference::Basic(name) => format!("{}:{}", get_name_of_original(components, component), name),
        StateVarReference::ArrayElement(name, index) => format!("{}:{}[{}]", get_name_of_original(components, component), name, index),
        StateVarReference::SizeOf(_) => panic!("size essential value"),
    }
}

/// Recurse until the original source is found
fn get_name_of_original(
    components: &HashMap<ComponentName, ComponentNode>,
    component: &ComponentNode
) -> String {
    match &component.copy_source {
        Some(CopySource::Component(source)) => get_name_of_original(components, components.get(source).unwrap()),
        _ => component.name.clone(),
    }
} 


/// Calculate all the (normal) state vars that depend on the given state var
fn get_state_variables_depending_on_me(
    core: &DoenetCore,
    component_name: &str,
    state_var_name: StateVarName,
) -> Vec<(ComponentName, StateVarReference)> {

    let mut depending_on_me = vec![];

    for (comp_name, comp_deps) in core.dependencies.iter() {
        for (sv_reference, sv_deps) in comp_deps {
            for (_, dep) in sv_deps {

                if let Dependency::StateVar(sv_dep) = dep {
                    if sv_dep.depends_on_objects.contains(&ObjectName::Component(component_name.to_string())) {
                        if sv_dep.depends_on_state_vars.contains(&state_var_name) {
                            depending_on_me.push((comp_name.clone(), *sv_reference));
                        }
                    }
                }
                // There should be no essential data that depends on normal state vars
            }
        }
    }

    depending_on_me

}

/// Iterate through each dependency as a tuple.
/// TODO: find if this is slower
fn _dependencies_iter(core: &DoenetCore)
    -> impl Iterator<Item=(&ComponentName, &StateVarReference, InstructionName, &Dependency)> {

    core.dependencies.iter()
        .flat_map(|(c_name, h1)| h1.iter()
            .flat_map(|(sv_ref, h2)| h2.iter()
                .map(|(i_name, dep)|
                    (c_name, sv_ref, *i_name, dep)
                ).collect::<Vec<(&String, &StateVarReference, &str, &Dependency)>>()
            ).collect::<Vec<(&String, &StateVarReference, &str, &Dependency)>>()
        )
}


    

fn resolve_state_variable(
    core: &DoenetCore,
    component: &ComponentNode,
    state_var_name: &StateVarReference,
) -> StateVarValue {
        
    let state_vars = core.component_states.get(&component.name).unwrap();

    // No need to continue if the state var is already resolved
    let current_state = state_vars.get(state_var_name).unwrap();
    if let State::Resolved(current_value) = current_state.get_state() {
        return current_value;
    }

    log_debug!("Resolving {}:{:?}", component.name, state_var_name);

    let mut dependency_values: HashMap<InstructionName, Vec<DependencyValue>> = HashMap::new();

    let my_dependencies = core.dependencies.get(&component.name).unwrap().get(state_var_name);

    
    for (dep_name, dep) in my_dependencies.unwrap_or(&HashMap::new()) {

        let mut values_for_this_dep: Vec<DependencyValue> = Vec::new();
        match dep {
            Dependency::StateVar(sv_dep) => {
                for depends_on in &sv_dep.depends_on_objects {

                    match depends_on {
                        ObjectName::String(string) => {

                            // The only thing to get from a string is its fake 'value' state var
                            if sv_dep.depends_on_state_vars.contains(&"value") {
                                values_for_this_dep.push(DependencyValue {
                                    component_type: "string",
                                    state_var_name: "value",
                                    value: StateVarValue::String(string.to_string()),
                                });
                       
                            }
                        },
                        ObjectName::Component(depends_on_name) => {

                            let depends_on_component = core.component_nodes.get(depends_on_name).unwrap();

                            for &dep_state_var_name in &sv_dep.depends_on_state_vars {

                                let depends_on_value = resolve_state_variable(core, depends_on_component, dep_state_var_name);

                                values_for_this_dep.push(DependencyValue {
                                    component_type: depends_on_component.component_type,
                                    state_var_name: dep_state_var_name,
                                    value: depends_on_value.clone(),
                                });

                            }
                        }
                    }
                }
            },
            Dependency::Essential(ess_dep) => {
                let value = core.essential_data.get(&ess_dep.depends_on_essential).unwrap().clone();

                values_for_this_dep.push(DependencyValue {

                    value: value.get_value(),

                    // We don't really need these fields in this case (?)
                    component_type: "essential_data",
                    state_var_name: "",
                })
            }
        }
        dependency_values.insert(dep_name, values_for_this_dep);
    }


    log_debug!("{}:{} dependency values: {:#?}", component.name, state_var_name, dependency_values);


    let update_instruction = generate_update_instruction_including_shadowing(
        component,state_var_name,dependency_values
    ).expect(&format!("Can't resolve [{}]:[{}] ({} component type)",
        component.name, state_var_name, component.component_type)
    );


    let new_value = handle_update_instruction(component, state_vars, state_var_name, update_instruction);

    return new_value;

}




fn mark_stale_state_var_and_dependencies(
    core: &DoenetCore,
    component: &ComponentNode,
    state_var_name: StateVarName)
{

    log_debug!("Marking stale {}:{}", component.name, state_var_name);

    let component_state = core.component_states.get(&component.name).unwrap();

    let state_var = component_state.get(state_var_name).unwrap();
    state_var.mark_stale();

    let depending_on_me = get_state_variables_depending_on_me(core, &component.name, state_var_name);
    
    for (depending_comp_name, depending_state_var) in depending_on_me {
        let depending_comp = core.component_nodes.get(&depending_comp_name).unwrap();

        mark_stale_state_var_and_dependencies(core, depending_comp, depending_state_var);
    }
}




fn mark_stale_essential_datum_and_dependencies(
    core: &DoenetCore,
    essential_var_name: &str
) {

    log_debug!("Marking stale essential {}", essential_var_name);

    // tuples of component and state var that depend on this essential datum
    let my_dependencies: Vec<(ComponentName, StateVarName)> = core.dependencies
        .iter()
        .flat_map(|(component_name, h)|
            h.iter()
            .flat_map(|(sv_name, deps)|
                deps.iter()
                .filter_map(|(_, d)| match d {
                    Dependency::Essential(ess_dep) => {
                        if ess_dep.depends_on_essential == essential_var_name {
                            Some((component_name.clone(), *sv_name))
                        } else {
                            None
                        }
                    },
                    _ => None,
                })
            )
        )
        .collect();

    for (component_name, state_var_name) in my_dependencies {
        let component = core.component_nodes.get(&component_name).unwrap();
        mark_stale_state_var_and_dependencies(core, &component, state_var_name);
    }
}



/// Sets the state var and returns the new value
fn handle_update_instruction<'a>(
    component: &'a ComponentNode,
    component_state_vars: &HashMap<StateVarName, StateVar>,
    name: StateVarName,
    instruction: StateVarUpdateInstruction<StateVarValue>
) -> StateVarValue {

    log_debug!("Updating state var {}:{}", component.name, name);


    let updated_value: StateVarValue;

    match instruction {
        StateVarUpdateInstruction::NoChange => {
            let current_value = component_state_vars.get(name).unwrap().get_state();

            if let State::Resolved(current_resolved_value) = current_value {
                // Do nothing. It's resolved, so we can use it as is
                updated_value = current_resolved_value;

            } else {
                panic!("Cannot use NoChange update instruction on a stale value");
            }

        },
        StateVarUpdateInstruction::SetValue(new_value) => {

            let new_state_var_value = new_value;
            set_state_var(component, component_state_vars, name, new_state_var_value.clone()).expect(
                &format!("Failed to set {}:{} while handling SetValue update instruction", component.name, name)
            );

            updated_value = new_state_var_value;
        }

    };

    log_debug!("State var updated to {}", updated_value);

    return updated_value;
}


fn set_state_var(
    component: &ComponentNode,
    component_state_vars: &HashMap<StateVarName, StateVar>,
    name: StateVarName,
    val: StateVarValue)
-> Result<(), String> {
    let state_var = component_state_vars.get(name).expect(
        &format!("Component {} of type {} does not have state var {}",
        component.name, component.component_type, name)
    );

    state_var.set_value(val)
}



#[derive(Debug)]
pub struct Action {
    pub component_name: ComponentName,
    pub action_name: String,

    /// The keys are not strictly state variable names.
    /// They are whatever name the renderer calls the new value.
    pub args: HashMap<String, StateVarValue>,
}


/// Internal struct used to track changes
#[derive(Debug, Clone)]
enum UpdateRequest {
    SetEssentialValue(String, StateVarValue),
    SetStateVar(ComponentName, StateVarName, StateVarValue),
}


/// Among other things, this produces info about the component name based on
/// the dependency instruction.
fn convert_dependency_values_to_update_request(
    core: &DoenetCore,
    component: &ComponentNode,
    state_var: StateVarName,
    requests: HashMap<InstructionName, Vec<DependencyValue>>
) -> Vec<UpdateRequest> {

    let instruction_names = core.dependencies
        .get(&component.name).unwrap()
        .get(state_var).unwrap();

    requests.iter()
        .flat_map(|(instruction_name, values)|
            values.iter()
                .map(|value|
                    match instruction_names.get(instruction_name).unwrap() {
                        Dependency::Essential(dep) => {
                            UpdateRequest::SetEssentialValue(
                                dep.depends_on_essential.clone(),
                                value.value.clone(),
                            )
                        },
                        Dependency::StateVar(_) => {
                            UpdateRequest::SetStateVar(
                                component.name.clone(),
                                value.state_var_name.clone(),
                                value.value.clone(),
                            )
                        },
                    }
                )
                .collect::<Vec<UpdateRequest>>()
        )
        .collect()
}

pub fn handle_action_from_json(core: &DoenetCore, action: &str) {
    
    let action = parse_json::parse_action_from_json(action)
        .expect(&format!("Error parsing json action: {}", action));

    log_debug!("Handling action {:#?}", action);

    // Apply alias to get the original component name
    let component_name = core.aliases.get(&action.component_name).unwrap_or(&action.component_name);

    let component = core.component_nodes.get(component_name)
        .expect(&format!("{} doesn't exist, but action {} uses it", action.component_name, action.action_name));

    let state_var_resolver = | state_var_name | {
        resolve_state_variable(core, component, state_var_name)
    };

    let state_vars_to_update = component.definition.on_action(&action.action_name, action.args, &state_var_resolver);

    for (state_var_name, requested_value) in state_vars_to_update {

        let request = UpdateRequest::SetStateVar(component_name.clone(), state_var_name, requested_value);
        process_update_request(core, component, state_var_name, &request);
    }
}


fn process_update_request(
    core: &DoenetCore,
    component: &ComponentNode,
    state_var_name: StateVarName,
    update_request: &UpdateRequest
) {

    log_debug!("Processing update request for {}:{}", component.name, state_var_name);

    match update_request {
        UpdateRequest::SetEssentialValue(key, requested_value) => {

            let essential_var = core.essential_data.get(key)
                .expect(&format!("'{}' is not in essential data {:#?}", key, core.essential_data));

            essential_var.set_value(requested_value.clone()).expect(
                &format!("Failed to set essential value for {}, {}", component.name, key)
            );

            log_debug!("Updated essential data {:#?}", core.essential_data);

            mark_stale_essential_datum_and_dependencies(core, &key);
        },

        UpdateRequest::SetStateVar(dep_comp_name, dep_state_var_name, requested_value) => {

            let dep_comp = core.component_nodes.get(dep_comp_name).unwrap();

            let dep_update_requests = request_dependencies_to_update_value_including_shadow(
                core,
                dep_comp,
                dep_state_var_name,
                requested_value.clone(),
            );

            for dep_update_request in dep_update_requests {
                process_update_request(core, dep_comp, dep_state_var_name, &dep_update_request);
            }

            mark_stale_state_var_and_dependencies(core, component, state_var_name);
        }
    }
}



pub fn update_renderers(core: &DoenetCore) -> String {
    let json_obj = generate_render_tree(core);
    serde_json::to_string(&json_obj).unwrap()
}


fn generate_render_tree(core: &DoenetCore) -> serde_json::Value {

    let root_node = core.component_nodes.get(&core.root_component_name).unwrap();
    let mut json_obj: Vec<serde_json::Value> = vec![];

    generate_render_tree_internal(core, root_node, &mut json_obj, None);

    serde_json::Value::Array(json_obj)
}

fn generate_render_tree_internal(
    core: &DoenetCore,
    component: &ComponentNode,
    json_obj: &mut Vec<serde_json::Value>,
    came_from_copy: Option<&ComponentName>,
) {

    let component_state = core.component_states.get(&component.name).unwrap();

    let comp_state_vars = component_state;

    use serde_json::json;

    let state_vars = component.definition.state_var_definitions();

    let renderered_state_vars = state_vars.into_iter().filter(|kv| match kv.1 {
        StateVarVariant::Integer(sv) => sv.for_renderer,
        StateVarVariant::Number(sv) =>  sv.for_renderer,
        StateVarVariant::String(sv) =>  sv.for_renderer,
        StateVarVariant::Boolean(sv) => sv.for_renderer,
    });

    let mut state_values = serde_json::Map::new();
    for (name, _variant) in renderered_state_vars {

        resolve_state_variable(core, component, name);

        let state_var_value = comp_state_vars.get(name).unwrap()
            .copy_value_if_resolved().unwrap();

        state_values.insert(name.to_string(), match state_var_value {
            StateVarValue::Integer(v) => json!(v),
            StateVarValue::Number(v) =>  json!(v),
            StateVarValue::String(v) =>  json!(v),
            StateVarValue::Boolean(v) => json!(v),
        });
    }

    let name_to_render = match &came_from_copy {
        Some(copy_name) => name_child_of_copy(&component.name, &copy_name),
        None => component.name.clone(),
    };

    let mut children_instructions = Vec::new();
    if component.definition.should_render_children() {
        for (child, actual_child) in get_children_including_copy(&core.component_nodes, component).iter() {
            match child {
                ComponentChild::Component(comp_name) => {
                    // recurse for children
                    let comp = core.component_nodes.get(comp_name).unwrap();
                    
                    let child_came_from_copy =
                        came_from_copy.or(
                            if *actual_child {
                                None
                            } else {
                                Some(&component.name)
                            });

                    generate_render_tree_internal(core, comp, json_obj, child_came_from_copy); 

                    let mut child_actions = serde_json::Map::new();

                    for action_name in comp.definition.action_names() {
                        child_actions.insert(action_name.to_string(), json!({
                            "actionName": action_name,
                            "componentName": comp.name,
                        }));
                    }

                    children_instructions.push(json!({
                        "actions": child_actions,
                        "componentName": comp.name,
                        "componentType": comp.component_type,
                        "effectiveName": comp.name,
                        "rendererType": render_type_of(comp.component_type),
                    }));
                },
                ComponentChild::String(string) => {
                    children_instructions.push(json!(string));
                },
            }
        }
    }

    json_obj.push(json!({
        "componentName": name_to_render,
        "stateValues": serde_json::Value::Object(state_values),
        "childrenInstructions": json!(children_instructions),
    }));

}

fn render_type_of(comp_type: &str) -> &str {
    match comp_type {
        "numberInput" => "textInput",
        _ => comp_type,
    }
}




////////////// Wrappers providing for CopySource and sequence component //////////////

/// This includes the copy source's children. The flag is false when it is
/// a copy source's child. Also skips sequence components.
fn get_children_including_copy(
    components: &HashMap<ComponentName, ComponentNode>,
    component: &ComponentNode
) -> Vec<(ComponentChild, bool)> {
    let mut children_vec: Vec<(ComponentChild, bool)> = Vec::new();
    if let Some(CopySource::Component(ref source)) = component.copy_source {

        let source_comp = components.get(source).unwrap();
        children_vec = get_children_including_copy(components, source_comp)
            .iter()
            .map(|(c, _)| (c.clone(), false))
            .collect();
    }

    children_vec.extend(
        component.children
        .iter()
        .map(|c| (c.clone(), true))
    );

    children_vec
}


fn return_dependency_instruction_including_shadowing(
    component: &ComponentNode,
    state_var: StateVarName,
) -> HashMap<InstructionName, DependencyInstruction> {

    if let Some((source_comp, source_state_var)) = state_var_is_shadowing(component, state_var) {

        HashMap::from([
            (SHADOW_INSTRUCTION_NAME, DependencyInstruction::StateVar(StateVarDependencyInstruction {
                component_name: Some(source_comp), //.clone(),
                state_var: source_state_var
            }))
        ])

    } else {
        let state_var_def = component.definition.state_var_definitions().get(state_var).unwrap();

        state_var_def.return_dependency_instructions(HashMap::new())
    }
}




fn generate_update_instruction_including_shadowing(
    component: &ComponentNode,
    state_var: StateVarName,
    dependency_values: HashMap<InstructionName, Vec<DependencyValue>>

) -> Result<StateVarUpdateInstruction<StateVarValue>, String> {

    if state_var_is_shadowing(component, state_var).is_some() {

        // Assuming that source state var is same type as this state var
        let source_value = dependency_values.dep_value(SHADOW_INSTRUCTION_NAME)?
            .has_exactly_one_element()?
            .value();

        Ok(StateVarUpdateInstruction::SetValue(source_value))

    } else {
        // Otherwise, this state var is not shadowing, so proceed normally
        let state_var_def = component.definition.state_var_definitions().get(state_var).unwrap();

        state_var_def.determine_state_var_from_dependencies(dependency_values)
    }
}



fn request_dependencies_to_update_value_including_shadow(
    core: &DoenetCore,
    component: &ComponentNode,
    state_var: StateVarName,
    new_value: StateVarValue,
) -> Vec<UpdateRequest> {
    if let Some((source_comp, source_state_var)) = state_var_is_shadowing(component, state_var) {

        vec![UpdateRequest::SetStateVar(source_comp, source_state_var, new_value)]

    } else {
        let requests = component.definition.state_var_definitions().get(state_var).unwrap()
            .request_dependencies_to_update_value(new_value);

        convert_dependency_values_to_update_request(core, component, state_var, requests)
    }
}

/// Detect if a state var is shadowing because of a CopySource
/// and has a primary input state variable, which is needed.
fn state_var_is_shadowing(component: &ComponentNode, state_var: StateVarName)
    -> Option<(ComponentName, StateVarName)> {

    if let Some(CopySource::StateVar(ref source_comp, source_state_var)) = component.copy_source {
        if let Some(primary_input_state_var) = component.definition.primary_input_state_var() {

            if state_var == primary_input_state_var {
                Some((source_comp.to_string(), source_state_var))
            } else {
                None
            }
        } else {
            panic!("{} component type doesn't have a primary input state var", component.component_type);
        }

    } else {
        None
    }
}
