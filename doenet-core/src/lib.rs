pub mod state_variables;
pub mod component;

pub mod state;
pub mod parse_json;
pub mod utils;
pub mod base_definitions;
pub mod math_expression;

use base_definitions::{PROP_INDEX_SV, prop_index_determine_value, get_children_of_type};
use lazy_static::lazy_static;
use parse_json::{DoenetMLError, DoenetMLWarning, MLComponent};
use state::StateForStateVar;
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

use state::{State, EssentialStateVar};
use component::*;
use state_variables::*;

use crate::math_expression::MathExpression;
use crate::utils::{log_json, log_debug};
use serde::Serialize;


/// A static DoenetCore is created from parsed DoenetML at the beginning.
/// While `component_states` and `essential_data` can update using
/// internal mutability (the RefCell), the over-arching HashMaps are static.
#[derive(Debug)]
pub struct DoenetCore {
    /// The component tree has almost the same structure as the tree of elements
    /// typed into DoenetML, except:
    /// - macros are converted into their own components
    pub component_nodes: HashMap<ComponentName, ComponentNode>,

    /// Keyed by
    /// - `ComponentName` not ComponentRef - a ComponentRef's state variables
    ///   point to the state variables of a ComponentName
    /// - `StateVarName` rather than `StateVarReference`
    ///   so that it is static even when arrays change size
    pub component_states: HashMap<ComponentName, HashMap<StateVarName, StateForStateVar>>,

    /// This should always be the name of a <document> component
    pub root_component_name: ComponentName,

    /// **The Dependency Graph**
    /// A DAC whose vertices are the state variables and attributes
    /// of every component, and whose endpoint vertices are essential data.
    ///
    /// Used for
    /// - producing values when determining a state variable
    /// - tracking when a change affects other state variables
    pub dependencies: HashMap<DependencyKey, Vec<Dependency>>,

    /// This determines which components a Collection includes
    pub group_dependencies: HashMap<ComponentName, Vec<CollectionMembers>>,

    /// Endpoints of the dependency graph.
    /// Every update instruction will lead to these.
    pub essential_data: HashMap<ComponentName, HashMap<EssentialDataOrigin, EssentialStateVar>>,
}


/// State variables are keyed by:
/// 1. the name of the component
/// 2. the name of a state variable slice
///    which allows for two kinds of dependencies:
///      - direct dependency: when a single state var depends on something
///      - indirect dependency: when a group depends on something,
///        and members of the group inherit the dependency.
///        The motivation for indirect dependencies is that
///        the size of groups can change (e.g. an array changes size).
///        To keep the dependency graph static, we do not update
///        individual dependencies but simply apply the group dependency.
/// 3. the instruction name, given by the state variable to track where
///    dependecy values came from.
#[derive(Debug, Hash, PartialEq, Eq, Serialize)]
pub enum DependencyKey {
    StateVar(ComponentName, StateVarSlice, InstructionName),
}

impl DependencyKey {
    fn component_name(&self) -> &str {
        match self {
            DependencyKey::StateVar(name, _, _) => name,
        }
    }
}


/// A collection of edges on the dependency tree
/// Groups and array state var slices get converted into multiple DependencyValues
#[derive(Debug, Clone, PartialEq, Eq, Serialize, enum_as_inner::EnumAsInner)]
pub enum Dependency {
    Essential {
        component_name: ComponentName,
        origin: EssentialDataOrigin,
    },

    // outer product of the members of the group and states in the slice
    StateVar {
        component_group: ComponentGroup,
        instance: RelativeInstance,
        state_var_slice: StateVarSlice,
    },
    // Necessary when a child dependency instruction encounters a groups
    // whose members replace themselves with children
    // - ex: <template> inside <map>
    // Implementation is WIP
    UndeterminedChildren {
        component_name: ComponentName,
        desired_profiles: Vec<ComponentProfile>,
    },
    MapSources {
        map_sources: ComponentName,
        state_var_slice: StateVarSlice,
    },
    StateVarArrayCorrespondingElement {
        component_group: ComponentGroup,
        array_state_var_name: StateVarName,
    },

    StateVarArrayDynamicElement {
        component_name: ComponentName,
        array_state_var_name: StateVarName,
        index_state_var: StateRef, // presumably an integer from the component that carries this dependency
    },
}

/// Defines which components form the members of a collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, enum_as_inner::EnumAsInner)]
pub enum CollectionMembers {
    Component(ComponentName),

    Batch(ComponentName),

    /// Only included if the boolean state var is true
    ComponentOnCondition {
        component_name: ComponentName,
        condition: StateRef, // a boolean
    },

    /// The members of this are the same component, but different instances
    InstanceBySources {
        template: ComponentName,
        sources: ComponentName, // a collection
    },

    // /// Points to collection of whose members are undetermined children
    // UndeterminedChildren(ComponentName),
}




pub fn create_doenet_core(
    program: &str,
    existing_essential_data: Option<HashMap<ComponentName, HashMap<EssentialDataOrigin, EssentialStateVar>>>,
) -> Result<(DoenetCore, Vec<DoenetMLWarning>), DoenetMLError> {

    // Create component nodes and attributes
    let (ml_components, component_attributes, root_component_name, map_sources_alias) =
        parse_json::create_components_tree_from_json(program)?;

    let mut doenet_ml_warnings = vec![];

    let component_nodes = convert_ml_components_into_component_nodes(ml_components, map_sources_alias, &mut doenet_ml_warnings)?;

    doenet_ml_warnings.extend(check_for_invalid_childen_component_profiles(&component_nodes));
    check_for_cyclical_copy_sources(&component_nodes)?;
    check_for_invalid_component_names(&component_nodes, &component_attributes)?;

    let group_dependencies = create_group_dependencies(&component_nodes);
    let (dependencies, essential_data) = create_dependencies_and_essential_data(
        &component_nodes,
        &component_attributes,
        existing_essential_data
    );
    check_for_cyclical_dependencies(&dependencies)?;

    let component_states = create_stale_component_states(&component_nodes);


    log_json!("Components upon core creation",
        utils::json_components(&component_nodes, &component_states));
    log_json!("Dependencies upon core creation",
        utils::json_dependencies(&dependencies));
    log_json!("Essential data upon core creation",
        utils::json_essential_data(&essential_data));
    log_json!("Group dependencies upon core creation",
        &group_dependencies);
    // log_debug!("DoenetCore creation warnings, {:?}", doenet_ml_warnings);
    Ok((DoenetCore {
        component_nodes,
        component_states,
        root_component_name,
        dependencies,
        group_dependencies,
        essential_data,
    }, doenet_ml_warnings))
}


/// Add CopySource info
fn convert_ml_components_into_component_nodes(
    ml_components: HashMap<ComponentName, MLComponent>,
    map_sources_alias: HashMap<String, String>,
    doenet_ml_warnings: &mut Vec<DoenetMLWarning>,
) -> Result<HashMap<ComponentName, ComponentNode>, DoenetMLError> {
    let mut component_nodes = HashMap::new();
    for (name, ml_component) in ml_components.iter() {
        
        let copy_source = copy_source_for_ml_component(
            &ml_components,
            ml_component,
            &map_sources_alias,
            doenet_ml_warnings,
        )?;

        let component_node = ComponentNode {
            name: name.clone(),
            parent: ml_component.parent.clone(),
            children: ml_component.children.clone(),
            copy_source,
            static_attributes: ml_component.static_attributes.clone(),
            definition: ml_component.definition,
        };

        component_nodes.insert(name.clone(), component_node);
    }

    Ok(component_nodes)
}

fn copy_source_for_ml_component(
    ml_components: &HashMap<ComponentName, MLComponent>,
    ml_component: &MLComponent,
    map_sources_alias: &HashMap<String, String>,
    doenet_ml_warnings: &mut Vec<DoenetMLWarning>,
) -> Result<Option<CopySource>, DoenetMLError> {

    let source_comp_name = ml_component.copy_source.as_ref();
    if source_comp_name.is_none() {
        return Ok(None);
    }
    let source_comp_name = source_comp_name.unwrap();

    if let Some(map_source) = map_sources_alias.get(source_comp_name) {
        return Ok(Some(CopySource::MapSources(map_source.to_string())));
    }

    let source_comp = ml_components
        .get(source_comp_name)
        .ok_or(DoenetMLError::ComponentDoesNotExist {
            comp_name: source_comp_name.clone()
        })?;

    let component_index = &ml_component.component_index;
    let (comp_ref, source_def) = match (component_index.len(), component_index.first()) {
        (1, Some(ObjectName::String(first_string))) => {

            // static index
            let string_value = first_string.parse().unwrap_or(0.0);
            let index: usize = convert_float_to_usize(string_value)
                .unwrap_or(0);

            if index == 0 {
                doenet_ml_warnings.push(DoenetMLWarning::PropIndexIsNotPositiveInteger {
                    comp_name: ml_component.name.clone(),
                    invalid_index: string_value.to_string()
                });
            }

            match (&ml_component.copy_collection, &source_comp.definition.replacement_components) {
                (None, Some(ReplacementComponents::Batch(def)))  => {
                    (ComponentRef::BatchMember(source_comp_name.clone(), None, index),
                    def.member_definition)
                },
                (None, Some(ReplacementComponents::Collection(def)))  => {
                    (ComponentRef::CollectionMember(source_comp_name.clone(), index),
                    (def.member_definition)(&source_comp.static_attributes))
                },
                (Some(key), _) => {
                    let (batch_name, batch_def)  = source_comp.definition.batches
                        .get_key_value_ignore_case(key).unwrap();
                    (ComponentRef::BatchMember(source_comp_name.clone(), Some(batch_name), index),
                    batch_def.member_definition)
                },
                (None, _)  => panic!("not a group"),
            }
        },
        (0, _) => {

            // no index
            (ComponentRef::Basic(source_comp_name.clone()), source_comp.definition)
        }
        (_, _) => {

            // dynamic index
            todo!("dynamic component index");
        },
    };

    let copy_instance = ml_component.copy_instance.clone().unwrap_or(RelativeInstance::default());

    let copy_prop = ml_component.copy_prop.as_ref();
    if copy_prop.is_none() {
        if !std::ptr::eq(ml_component.definition, source_def) {
            return Err(DoenetMLError::ComponentCannotCopyOtherType {
                component_name: ml_component.name.clone(),
                component_type: ml_component.definition.component_type,
                source_type: &source_def.component_type,
            });
        }

        return Ok(Some(CopySource::Component(comp_ref, copy_instance)));
    }
    let copy_prop = copy_prop.unwrap();

    if let Some(state_ref) = source_def.array_aliases.get(copy_prop.as_str()) {
        return Ok(Some(CopySource::StateVar(comp_ref, copy_instance, state_ref.clone())))
    }

    let source_sv_name = source_def
        .state_var_definitions
        .get_key_value_ignore_case(copy_prop.as_str())
        .ok_or(DoenetMLError::StateVarDoesNotExist {
            comp_name: source_comp.name.clone(),
            sv_name: copy_prop.clone(),
        })?
        .0;

    let source_sv_def = source_def
        .state_var_definitions
        .get(source_sv_name)
        .unwrap();

    let prop_index = &ml_component.prop_index;
    match (prop_index.len(), prop_index.first()) {
        (1, Some(ObjectName::String(first_string))) => {

            // static index
            let string_value = first_string.parse().unwrap_or(0.0);
            let index: usize = convert_float_to_usize(string_value)
                .unwrap_or(0);

            if index == 0 {
                doenet_ml_warnings.push(DoenetMLWarning::PropIndexIsNotPositiveInteger {
                    comp_name: ml_component.name.clone(),
                    invalid_index: string_value.to_string()
                });
            }

            if !source_sv_def.is_array() {
                return Err(DoenetMLError::CannotCopyIndexForStateVar {
                    source_comp_name: comp_ref.name(),
                    source_sv_name,
                });
            }

            Ok(Some(CopySource::StateVar(
                comp_ref,
                copy_instance,
                StateRef::ArrayElement(source_sv_name, index)
            )))
        },
        (0, _) => {

            // no index
            if source_sv_def.is_array() {
                return Err(DoenetMLError::CannotCopyArrayStateVar {
                    source_comp_name: comp_ref.name(),
                    source_sv_name,
                });
            }
            Ok(Some(CopySource::StateVar(
                comp_ref,
                copy_instance,
                StateRef::Basic(source_sv_name)
            )))
        },
        (_, _) => {

            // dynamic index
            let variable_components = ml_component.prop_index.iter()
                .filter_map(|obj| obj.as_component().map(|c| c.clone()))
                .collect();

            Ok(Some(CopySource::DynamicElement(
                comp_ref.name(),
                source_sv_name,
                MathExpression::new(&ml_component.prop_index),
                variable_components,
            )))
        },
    }
}


fn create_group_dependencies(component_nodes: &HashMap<ComponentName, ComponentNode>) -> HashMap<ComponentName, Vec<CollectionMembers>> {
    let mut group_dependencies = HashMap::new();
    for (component_name, component) in component_nodes.iter() {
        if let Some(ReplacementComponents::Collection(group_def)) = &component.definition.replacement_components {

            let deps = (group_def.group_dependencies)(
                &component,
                &component_nodes,
            );
            group_dependencies.insert(component_name.clone(), deps);
        }
    }

    // flatten so that collections do not point to other collections
    fn flat_members_for_collection(
        component_nodes: &HashMap<ComponentName, ComponentNode>,
        group_dependencies: &HashMap<ComponentName, Vec<CollectionMembersOrCollection>>,
        component: &ComponentName,
    ) -> Vec<CollectionMembers> {
        group_dependencies.get(component).unwrap()
            .iter()
            .flat_map(|c| {
                match c {
                    CollectionMembersOrCollection::Collection(c) =>
                        flat_members_for_collection(component_nodes, group_dependencies, c),
                    CollectionMembersOrCollection::Members(m) => vec![m.clone()],
                }
            }).collect()
    }

    let group_dependencies =  group_dependencies.iter()
        .map(|(k, _v)| (k.clone(), flat_members_for_collection(component_nodes, &group_dependencies, k)))
        .collect();

    group_dependencies
}


fn create_dependencies_and_essential_data(
    component_nodes: &HashMap<ComponentName, ComponentNode>,
    component_attributes: &HashMap<ComponentName, HashMap<AttributeName, HashMap<usize, Vec<ObjectName>>>>,
    existing_essential_data: Option<HashMap<ComponentName, HashMap<EssentialDataOrigin, EssentialStateVar>>>,
) -> (HashMap<DependencyKey, Vec<Dependency>>, HashMap<ComponentName, HashMap<EssentialDataOrigin, EssentialStateVar>>) {

    let mut all_state_var_defs: Vec<(&ComponentName, StateVarName, &StateVarVariant)> = Vec::new();
    for (_, comp) in component_nodes.iter() {
        for (sv_name, sv_def) in comp.definition.state_var_definitions {
            all_state_var_defs.push((&comp.name, sv_name, sv_def));
        }
    }

    let mut element_specific_dependencies: HashMap<(ComponentRef, StateVarName), Vec<usize>> = HashMap::new();

    for (comp_name, sv_name, sv_def) in all_state_var_defs {
        if !sv_def.is_array() {
            continue;
        }

        let comp = component_nodes.get(comp_name).unwrap();

        let possible_attributes = if let Some(my_own_comp_attrs) = component_attributes.get(comp_name) {
            Some(my_own_comp_attrs)
        } else if let Some(CopySource::Component(..)) = comp.copy_source {
            let (final_source, _) = get_recursive_copy_source_component_when_exists(&component_nodes, comp);
            component_attributes.get(final_source)
        } else {
            None
        };

        if let Some(attribute_for_comp) = possible_attributes {

            if let Some(attribute_for_sv) = attribute_for_comp.get(sv_name) {
                let element_dep_flags: Vec<usize> = attribute_for_sv.iter().map(|(id, _)| *id).collect();
                element_specific_dependencies.insert(
                    (ComponentRef::Basic(comp_name.to_string()), sv_name),
                    element_dep_flags
                );
            }
        }
    }

    // Fill in component_states and dependencies HashMaps for every component
    // and supply any essential_data required by dependencies.
    let should_initialize_essential_data = existing_essential_data.is_none();
    let mut essential_data = existing_essential_data.unwrap_or(HashMap::new());

    let mut dependencies = HashMap::new();

    for (component_name, component_node) in component_nodes.iter() {

        let dependencies_for_this_component = create_all_dependencies_for_component(
            &component_nodes,
            component_node,
            component_attributes.get(component_name).unwrap_or(&HashMap::new()),
            // copy_index_flags.get(component_name).as_deref(),
            &mut essential_data,
            should_initialize_essential_data,
            &element_specific_dependencies,
        );
        dependencies.extend(dependencies_for_this_component);



    }
    (dependencies, essential_data)
}


fn create_all_dependencies_for_component(
    components: &HashMap<ComponentName, ComponentNode>,
    component: &ComponentNode,
    component_attributes: &HashMap<AttributeName, HashMap<usize, Vec<ObjectName>>>,
    // copy_index_flag: Option<&(ComponentName, StateVarName, Vec<ObjectName>)>,
    essential_data: &mut HashMap<ComponentName, HashMap<EssentialDataOrigin, EssentialStateVar>>,
    should_initialize_essential_data: bool,
    element_specific_dependencies: &HashMap<(ComponentRef, StateVarName), Vec<usize>>,
) -> HashMap<DependencyKey, Vec<Dependency>> {

    // log_debug!("Creating dependencies for {}", component.name);
    let mut dependencies: HashMap<DependencyKey, Vec<Dependency>> = HashMap::new();
    let my_definitions = component.definition.state_var_definitions;

    if let Some(CopySource::DynamicElement(_, _, ref expression, ref variable_components)) = component.copy_source {
        // We can't immediately figure out the index, so we need to use the state
        // var propIndex
        dependencies.extend(
            create_prop_index_dependencies(component, expression, variable_components, essential_data)
        );
    }

    for (&state_var_name, state_var_variant) in my_definitions {

        if state_var_variant.is_array() {

            let size_dep_instructions = state_var_variant
                .return_size_dependency_instructions(HashMap::new());

            for (instruct_name, ref dep_instruction) in size_dep_instructions.into_iter() {
                let instruct_dependencies = create_dependencies_from_instruction(
                    &components,
                    component,
                    component_attributes,
                    &StateVarSlice::Single(StateRef::SizeOf(state_var_name)),
                    dep_instruction,
                    instruct_name,
                    essential_data,
                    should_initialize_essential_data,
                );

                dependencies.insert(
                    DependencyKey::StateVar(
                        component.name.clone(),
                        StateVarSlice::Single(StateRef::SizeOf(state_var_name)),
                        instruct_name
                    ),
                    instruct_dependencies,
                );

            }

            let array_dep_instructions = state_var_variant
                .return_array_dependency_instructions(HashMap::new());

            for (instruct_name, ref dep_instruction) in array_dep_instructions.into_iter() {
                let instruct_dependencies =
                    create_dependencies_from_instruction(
                        &components,
                        component,
                        component_attributes,
                        &StateVarSlice::Array(state_var_name),
                        dep_instruction,
                        instruct_name,
                        essential_data,
                        should_initialize_essential_data
                    );

                dependencies.insert(
                    DependencyKey::StateVar(
                        component.name.clone(),
                        StateVarSlice::Array(state_var_name),
                        instruct_name,
                    ),
                    instruct_dependencies,
                );
            }

            // make dependencies for elements when size has an essential value
            // let elements = {
                // let source_comp_name = get_essential_data_component_including_copy(components, component);

                // let size = essential_data
                //     .get(source_comp_name)
                //     .and_then(|c| c
                //         .get(&EssentialDataOrigin::StateVar(state_var_name))
                //         .and_then(|s| s
                //             .get_value(StateIndex::SizeOf)
                //             .and_then(|v|
                //                 usize::try_from(v).ok()
                //             )
                //         )
                //     ).unwrap_or(0);

                // indices_for_size(size)
            // };
            let empty = &Vec::new();

            let elements = element_specific_dependencies.get(&(ComponentRef::Basic(component.name.clone()), state_var_name)).unwrap_or(empty);

            // TODO: change this hack
            let mut elements = elements.clone();
            if !elements.contains(&1) {
                elements.push(1)
            }
            if !elements.contains(&2) {
                elements.push(2)
            }

            log_debug!("Will make dependencies for elements {:?} of {}:{}", elements, component.name, state_var_name);

            for index in elements {

                let element_dep_instructions = state_var_variant
                    .return_element_dependency_instructions(index, HashMap::new());

                for (instruct_name, ref dep_instruction) in element_dep_instructions.into_iter() {
                    let instruct_dependencies =
                        create_dependencies_from_instruction(
                            &components,
                            component,
                            component_attributes,
                            &StateVarSlice::Single(StateRef::ArrayElement(state_var_name, index)),
                            dep_instruction,
                            instruct_name,
                            essential_data,
                            should_initialize_essential_data
                        );

                    dependencies.insert(
                        DependencyKey::StateVar(
                            component.name.clone(),
                            StateVarSlice::Single(StateRef::ArrayElement(state_var_name, index)),
                            instruct_name,
                        ),
                        instruct_dependencies,
                    );
                }
            }


        } else {

            let dependency_instructions = state_var_variant.return_dependency_instructions(HashMap::new());

            for (instruct_name, ref dep_instruction) in dependency_instructions.into_iter() {
                let instruct_dependencies = create_dependencies_from_instruction(
                    &components,
                    component,
                    component_attributes,
                    &StateVarSlice::Single(StateRef::Basic(state_var_name)),
                    dep_instruction,
                    instruct_name,
                    essential_data,
                    should_initialize_essential_data
                );

                dependencies.insert(
                    DependencyKey::StateVar(
                        component.name.clone(),
                        StateVarSlice::Single(StateRef::Basic(state_var_name)),
                        instruct_name,
                    ),
                    instruct_dependencies   
                );
            }

        }
    }

    dependencies

}


/// This function also creates essential data when a DependencyInstruction asks for it.
/// The second return is element specific dependencies.
fn create_dependencies_from_instruction(
    components: &HashMap<ComponentName, ComponentNode>,
    component: &ComponentNode,
    component_attributes: &HashMap<AttributeName, HashMap<usize, Vec<ObjectName>>>,
    state_var_slice: &StateVarSlice,
    instruction: &DependencyInstruction,
    instruction_name: InstructionName,
    essential_data: &mut HashMap<ComponentName, HashMap<EssentialDataOrigin, EssentialStateVar>>,
    should_initialize_essential_data: bool,
) -> Vec<Dependency> {

    log_debug!("Creating dependency {}:{}:{} from instruction {:?}", component.name, state_var_slice, instruction_name, instruction);

    let mut dependencies: Vec<Dependency> = Vec::new();

    match &instruction {

        DependencyInstruction::Essential { prefill } => {

            let (source_comp_name, _) = get_recursive_copy_source_component_when_exists(components, component);
            let essential_origin = EssentialDataOrigin::StateVar(state_var_slice.name());

            dependencies.push(Dependency::Essential {
                component_name: source_comp_name.clone(),
                origin: essential_origin.clone(),
            });

            if should_initialize_essential_data && source_comp_name == &component.name {
                // Components only create their own essential data

                let sv_def = component.definition.state_var_definitions.get(state_var_slice.name()).unwrap();

                let initial_data: StateVarValue = prefill
                    .and_then(|prefill_attr_name| component_attributes
                        .get(prefill_attr_name)
                        .and_then(|attr| {
                            attr.get(&1).unwrap()
                                .first().unwrap()
                                .as_string().and_then(|actual_str|
                                    package_string_as_state_var_value(actual_str.to_string(), sv_def).ok(),
                                )
                            })
                        )
                    .unwrap_or(sv_def.initial_essential_value());

                let initial_data = if sv_def.is_array() {
                    InitialEssentialData::Array(Vec::new(), initial_data)
                } else {
                    InitialEssentialData::Single(initial_data)
                };
    
                create_essential_data_for(
                    &source_comp_name,
                    essential_origin,
                    initial_data,
                    essential_data
                );

            }
        },

        DependencyInstruction::StateVar { component_ref, state_var } => {

            let component_ref = match component_ref {
                Some(name) => name.clone(),
                None => ComponentRef::Basic(component.name.clone()),
            };

            dependencies.push(
                Dependency::StateVar {
                    component_group: ComponentGroup::Single(component_ref),
                    instance: RelativeInstance::default(),
                    state_var_slice: state_var.clone(),
                }
            );

        },

        DependencyInstruction::CorrespondingElements { component_ref, array_state_var_name } => {

            let component_ref = match component_ref {
                Some(name) => name.clone(),
                None => ComponentRef::Basic(component.name.clone()),
            };

            dependencies.push(
                Dependency::StateVarArrayCorrespondingElement {
                    component_group: ComponentGroup::Single(component_ref),
                    array_state_var_name,
                }
            );

        },

        DependencyInstruction::Child { desired_profiles, parse_into_expression } => {

            enum RelevantChild<'a> {
                StateVar(Dependency),
                String(&'a String, &'a ComponentName), // value, parent name
            }

            let mut relevant_children: Vec<RelevantChild> = Vec::new();
            let mut can_parse_into_expression = *parse_into_expression;
            
            let (source_copy_root_name, source_relative_map) =
                get_recursive_copy_source_component_when_exists(components, component);
            let source = components.get(source_copy_root_name).unwrap();
            
            if let Some(CopySource::StateVar(ref source_comp_ref, ref instance, ref source_sv_ref)) = source.copy_source {
                // copying a state var means we don't inheret its children,
                // so we depend on it directly
                relevant_children.push(
                    RelevantChild::StateVar(Dependency::StateVar {
                        component_group: ComponentGroup::Single(source_comp_ref.clone()),
                        instance: instance.clone(),
                        state_var_slice: StateVarSlice::Single(source_sv_ref.clone())
                    })
                );
            } else if let Some(CopySource::DynamicElement(ref source_comp, source_sv, _, _)) = source.copy_source {
                relevant_children.push(
                    RelevantChild::StateVar(Dependency::StateVarArrayDynamicElement {
                        component_name: source_comp.clone(),
                        array_state_var_name: source_sv,
                        index_state_var: StateRef::Basic(PROP_INDEX_SV),
                    })
                );
            } else if let Some(CopySource::Component(ref source_comp_ref, ref instance)) = source.copy_source {
                if matches!(source_comp_ref, ComponentRef::BatchMember(_, _, _)) {
                    // a batch member has no children, so we depend on it directly
                    relevant_children.push(
                        RelevantChild::StateVar(Dependency::StateVar {
                            component_group: ComponentGroup::Single(source_comp_ref.clone()),
                            instance: instance.clone(),
                            state_var_slice: state_var_slice.clone(),
                        })
                    );
                }
            } else if let Some(CopySource::MapSources(map_sources)) = &component.copy_source {
                relevant_children.push(
                    RelevantChild::StateVar(Dependency::MapSources {
                        map_sources: map_sources.clone(),
                        state_var_slice: state_var_slice.clone(),
                    })
                );
            }


            let children = get_children_including_copy(components, component);

            for child in children.iter() {

                match child {
                    (ComponentChild::Component(child_name), _) => {

                        let child_node = components.get(child_name).unwrap();

                        let child_group = match child_node.definition.replacement_components {
                            Some(ReplacementComponents::Batch(_)) => ComponentGroup::Batch(child_name.clone()),
                            Some(ReplacementComponents::Collection(_)) => ComponentGroup::Collection(child_name.clone()),
                            Some(ReplacementComponents::Children) => panic!("replace children outside group, not implemented"),
                            None => ComponentGroup::Single(ComponentRef::Basic(child_name.clone())),
                        };
                        let child_def = group_member_definition(
                            components,
                            &child_group
                        );

                        if matches!(child_def.replacement_components, Some(ReplacementComponents::Children)) {
                            // cannot permanently parse into an expression when the type and number of children could change
                            can_parse_into_expression = false;
                            relevant_children.push(
                                RelevantChild::StateVar(Dependency::UndeterminedChildren {
                                    component_name: child_group.name(),
                                    desired_profiles: desired_profiles.clone(),
                                })
                            );
                        }

                        if let Some(profile_sv_slice) = child_def.component_profile_match(desired_profiles) {
                            relevant_children.push(
                                RelevantChild::StateVar(Dependency::StateVar {
                                    component_group: child_group,
                                    instance: source_relative_map.clone(),
                                    state_var_slice: profile_sv_slice,
                                })
                            );
                        }
                    },
                    (ComponentChild::String(string_value), actual_parent) => {
                        if desired_profiles.contains(&ComponentProfile::Text)
                            || desired_profiles.contains(&ComponentProfile::Number) {
                            relevant_children.push(
                                RelevantChild::String(string_value, &actual_parent.name)
                            );
                        }
                    },
                }
            }


            if can_parse_into_expression {

                // Assuming for now that expression is math expression
                let expression = MathExpression::new(
                    &relevant_children.iter().map(|child| match child {
                        // The component name doesn't matter, the expression just needs to know there is
                        // an external variable at that location
                        RelevantChild::StateVar(_) => ObjectName::Component(String::new()),
                        RelevantChild::String(string_value, _) => ObjectName::String(string_value.to_string()),
                    }).collect()
                );

                // Assuming that no other child instruction exists which has already filled
                // up the child essential data
                let essential_origin = EssentialDataOrigin::ComponentChild(0);

                if should_initialize_essential_data {
                    create_essential_data_for(
                        &component.name,
                        essential_origin.clone(),
                        InitialEssentialData::Single(
                            StateVarValue::MathExpr(expression),
                        ),
                        essential_data
                    );    
                }

                dependencies.push(Dependency::Essential {
                    component_name: component.name.clone(), origin: essential_origin,
                });

                // We already dealt with the essential data, so now only retain the component children
                relevant_children.retain(|child| matches!(child, RelevantChild::StateVar(_)));
                
            }

            // Stores how many string children added per parent.
            let mut essential_data_numbering: HashMap<ComponentName, usize> = HashMap::new();

            for relevant_child in relevant_children {

                match relevant_child {

                    RelevantChild::StateVar(child_dep) => {
                        dependencies.push(child_dep);
                    },

                    RelevantChild::String(string_value, actual_parent) => {
                        let index = essential_data_numbering
                            .entry(actual_parent.clone()).or_insert(0 as usize);

                        let essential_origin = EssentialDataOrigin::ComponentChild(*index);

                        if should_initialize_essential_data && &component.name == actual_parent {
                            // Components create their own essential data

                            let value = StateVarValue::String(string_value.clone());
                            create_essential_data_for(
                                actual_parent,
                                essential_origin.clone(),
                                InitialEssentialData::Single(value),
                                essential_data
                            );
                        }

                        dependencies.push(Dependency::Essential {
                            component_name: actual_parent.clone(),
                            origin: essential_origin,
                        });

                        *index += 1;

                    },

                }
            }
            

        },
        DependencyInstruction::Parent { state_var } => {

            let desired_state_var = state_var;

            let parent_name = component.parent.clone().expect(&format!(
                "Component {} doesn't have a parent, but the dependency instruction {}:{} asks for one.",
                    component.name, state_var_slice, instruction_name
            ));

            let parent_component = components.get(&parent_name).unwrap();

            // Look up what kind of child state var it is
            // If the state var is an array, depend on the array, otherwise as normal

            let sv_def = parent_component.definition.state_var_definitions.get(desired_state_var).unwrap();

            dependencies.push(Dependency::StateVar {
                component_group: ComponentGroup::Single(ComponentRef::Basic(parent_name.to_string())),
                instance: RelativeInstance::default(),
                state_var_slice: if sv_def.is_array() {
                    StateVarSlice::Array(desired_state_var)
                } else {
                    StateVarSlice::Single(StateRef::Basic(desired_state_var))
                }
            });

        },


        DependencyInstruction::Attribute { attribute_name, index } => {

            log_debug!("Getting attribute {} for {}:{}", attribute_name, component.name, state_var_slice);
            let state_var_name = state_var_slice.name();
            let state_var_ref = StateRef::from_name_and_index(state_var_name, *index);
            let sv_def = component.definition.state_var_definitions.get(state_var_name).unwrap();
            let essential_origin = EssentialDataOrigin::StateVar(state_var_name);


            let default_value = match sv_def {

                StateVarVariant::NumberArray(_)| 
                StateVarVariant::Number(_) | 
                StateVarVariant::Integer(_) => {
                    StateVarValue::MathExpr(MathExpression::new(
                        &vec![ObjectName::String(match sv_def.initial_essential_value() {
                            StateVarValue::Number(v) => v.to_string(),
                            StateVarValue::Integer(v) => v.to_string(),
                            _ => unreachable!(),
                        })]
                    ))
                },
                _ => sv_def.initial_essential_value(),
            };


            let get_value_from_object_list = |obj_list: &Vec<ObjectName>| -> StateVarValue {

                if matches!(sv_def, StateVarVariant::Number(_)
                    | StateVarVariant::NumberArray(_)
                    | StateVarVariant::Integer(_)
                    | StateVarVariant::Boolean(_)
                ) {
                    StateVarValue::MathExpr(
                        MathExpression::new(obj_list)
                    )
                } else if obj_list.len() > 0 {

                    let first_obj = obj_list.get(0).unwrap();
                    if obj_list.len() > 1 {
                        unimplemented!("Multiple objects for non mathexpression state var");
                    }
                    match first_obj {
                        ObjectName::String(str_val) => {
                            package_string_as_state_var_value(str_val.to_string(), sv_def).unwrap()
                        }
                        _ => default_value.clone()
                    }
                } else {
                    default_value.clone()
                }
            };

            if let Some(attribute) = component_attributes.get(*attribute_name) {
                // attribute specified
                log_debug!("attribute {:?}", attribute);

                // Create the essential data if it does not exist yet
                if should_initialize_essential_data && !essential_data_exists_for(&component.name, &essential_origin, essential_data) {

                    if sv_def.is_array() {

                        let mut essential_attr_objs: Vec<StateVarValue> = Vec::new();
                        
                        for (id, obj_list) in attribute {

                            let value = get_value_from_object_list(obj_list);

                            if *id > essential_attr_objs.len() {
                                essential_attr_objs.resize(*id, default_value.clone());
                            }
                            essential_attr_objs[id - 1] = value;

                        }

                        log_debug!("essential attributes {:?}", essential_attr_objs);

                        let initial_essential_data = InitialEssentialData::Array(essential_attr_objs, default_value);

                        create_essential_data_for(
                            &component.name,
                            essential_origin.clone(),
                            initial_essential_data,
                            essential_data,
                        );

                    } else {

                        assert_eq!(attribute.keys().len(), 1);
                        let obj_list = attribute.get(&1).unwrap();

                        log_debug!("Initializing non-array essential data for {}:{} from attribute data {:?}", component.name, state_var_name, obj_list);

                        let value = get_value_from_object_list(obj_list);
                        
                        create_essential_data_for(
                            &component.name,
                            essential_origin.clone(),
                            InitialEssentialData::Single(value),
                            essential_data,
                        );
                    };
                }



                if let StateIndex::SizeOf = index {

                    dependencies.push(Dependency::Essential {
                        component_name: component.name.clone(),
                        origin: essential_origin,
                    });
                
                } else {

                    let attribute_index = match index {
                        StateIndex::Element(i) => *i,
                        _ => 1,
                    };

                    let attr_objects = attribute.get(&attribute_index)
                        .expect(&format!("attribute {}:{} does not have index {}. Attribute: {:?}",
                            &component.name, attribute_name, &attribute_index, attribute));

                    let relevant_attr_objects = if matches!(sv_def,
                        StateVarVariant::Number(_) | StateVarVariant::NumberArray(_) | StateVarVariant::Integer(_)
                    ) {
                        // First add an essential dependency to the expression
                        dependencies.push(Dependency::Essential {
                            component_name: component.name.clone(),
                            origin: essential_origin.clone(),
                        });

                        attr_objects.into_iter().filter_map(
                            |obj| if matches!(obj, ObjectName::Component(_)) {
                                Some(obj.clone())
                            } else {
                                None
                            }
                        ).collect()
                    } else {
                        attr_objects.clone()
                    };

                    for attr_object in relevant_attr_objects {

                        let dependency = match attr_object {
                            ObjectName::String(_) => Dependency::Essential {
                                component_name: component.name.clone(),
                                origin: essential_origin.clone(),
                            },
                            ObjectName::Component(comp_name) => {
                                let comp = components.get(&comp_name).unwrap();
                                let primary_input_sv = comp.definition.primary_input_state_var.expect(
                                    &format!("An attribute cannot depend on a non-primitive component. Try adding '.value' to the macro.")
                                );
    
                                Dependency::StateVar {
                                    component_group: ComponentGroup::Single(ComponentRef::Basic(comp_name.clone())),
                                    instance: RelativeInstance::default(),
                                    state_var_slice: StateVarSlice::Single(StateRef::Basic(primary_input_sv)),
                                }
                            },
                        };

                        dependencies.push(dependency);
                    }
                    
                }
            } else if let Some(CopySource::Component(c, instance)) = &component.copy_source {

                // inherit attribute from copy source
                dependencies.push(Dependency::StateVar {
                    component_group: ComponentGroup::Single(c.clone()),
                    instance: instance.clone(),
                    state_var_slice: StateVarSlice::Single(state_var_ref),
                });
            } else {

                // let value = get_value_from_object_list(&vec![]);

                if should_initialize_essential_data {
                    create_essential_data_for(
                        &component.name,
                        EssentialDataOrigin::StateVar(state_var_name),
                        InitialEssentialData::Single(default_value),
                        essential_data
                    );    
                }

                dependencies.push(Dependency::Essential {
                    component_name: component.name.clone(),
                    origin: essential_origin,
                });

            }
        },

    }
    dependencies
}

fn create_prop_index_dependencies(
    component: &ComponentNode,
    math_expression: &MathExpression,
    variable_components: &Vec<ComponentName>,
    essential_data: &mut HashMap<ComponentName, HashMap<EssentialDataOrigin, EssentialStateVar>>,
)
-> HashMap<DependencyKey, Vec<Dependency>> {
    use base_definitions::*;

    let mut dependencies = HashMap::new();

    // Dependencies on source components for propIndex
    dependencies.insert(
        DependencyKey::StateVar(
            component.name.clone(),
            StateVarSlice::Single(StateRef::Basic(PROP_INDEX_SV)),
            PROP_INDEX_VARS_INSTRUCTION
        ),
        variable_components.iter().map(|comp_name| {
            Dependency::StateVar {
                component_group: ComponentGroup::Single(ComponentRef::Basic(comp_name.to_string())),
                instance: RelativeInstance::default(),
                state_var_slice: StateVarSlice::Single(StateRef::Basic("value")),
            }
        }).collect()
    );

    let origin = EssentialDataOrigin::StateVar(PROP_INDEX_SV);

    create_essential_data_for(
        &component.name,
        origin.clone(),
        InitialEssentialData::Single(StateVarValue::MathExpr(math_expression.clone())),
        essential_data,
    );

    // Dependency on math expression for propIndex
    dependencies.insert(
        DependencyKey::StateVar(
            component.name.clone(),
            StateVarSlice::Single(StateRef::Basic(PROP_INDEX_SV)),
            PROP_INDEX_EXPR_INSTRUCTION
        ),
        vec![Dependency::Essential {
            component_name: component.name.clone(),
            origin,
        }]
    );

    dependencies
}


fn package_string_as_state_var_value(input_string: String, state_var_variant: &StateVarVariant) -> Result<StateVarValue, String> {

    match state_var_variant {
        StateVarVariant::StringArray(_) |
        StateVarVariant::String(_) => {
            Ok(StateVarValue::String(input_string))
        },

        StateVarVariant::Boolean(_) => {

            if input_string == "true" {
                Ok(StateVarValue::Boolean(true))
            } else if input_string == "false" {
                Ok(StateVarValue::Boolean(false))
            } else {
                Err(format!("Cannot evaluate string '{}' as boolean", input_string))
            }
        },

        StateVarVariant::Integer(_) => {
            if let Ok(val) = evalexpr::eval_int(&input_string) {
                Ok(StateVarValue::Integer(val))
            } else {
                Err(format!("Cannot package string '{}' as integer", input_string))
        }
        },

        StateVarVariant::NumberArray(_) |
        StateVarVariant::Number(_) => {
            if let Ok(val) = evalexpr::eval_number(&input_string) {
                Ok(StateVarValue::Number(val))
            } else {
                Err(format!("Cannot package string '{}' as number", input_string))
            }
        },
    }
}


/// Essential data can be generated by
/// - a state variable requesting it
/// - a string child, converted into essential data
///   so that it can change when requested
/// - a string in an attribute
#[derive(Serialize, Debug, Clone, Eq, Hash, PartialEq)]
pub enum EssentialDataOrigin {
    StateVar(StateVarName),
    ComponentChild(usize),
    // AttributeString(usize),
}

enum InitialEssentialData {
    Single(StateVarValue),
    Array(Vec<StateVarValue>, StateVarValue),
}

/// Add essential data for a state variable or string child
fn create_essential_data_for(
    component_name: &ComponentName,
    origin: EssentialDataOrigin,
    initial_values: InitialEssentialData,
    essential_data: &mut HashMap<ComponentName, HashMap<EssentialDataOrigin, EssentialStateVar>>,
) {

    if let Some(comp_essential_data) = essential_data.get(component_name) {
        assert!( !comp_essential_data.contains_key(&origin) );
    }

    let essential_state = match initial_values {
        InitialEssentialData::Single(value) =>
            EssentialStateVar::new_single_basic_with_state_var_value(value),
        InitialEssentialData::Array(values, default_fill_value) =>
            EssentialStateVar::new_array_with_state_var_values(values, default_fill_value),
    };

    log_debug!("New essential data for {} {:?} {:?}", component_name, origin, essential_state);

    essential_data
        .entry(component_name.clone())
        .or_insert(HashMap::new())
        .entry(origin.clone())
        .or_insert(essential_state);
}

fn essential_data_exists_for(
    component_name: &ComponentName,
    origin: &EssentialDataOrigin,
    essential_data: &HashMap<ComponentName, HashMap<EssentialDataOrigin, EssentialStateVar>>
) -> bool {

    if let Some(comp_essen) = essential_data.get(component_name) {
        if let Some(_) = comp_essen.get(origin) {
            true
        } else {
            false
        }
    } else {
        false
    }
}


fn create_stale_component_states(component_nodes: &HashMap<ComponentName, ComponentNode>)
    -> HashMap<ComponentName, HashMap<StateVarName, StateForStateVar>> {

    let mut component_states = HashMap::new();
    for (component_name, component_node) in component_nodes.iter() {
        let mut state_for_this_component: HashMap<StateVarName, StateForStateVar> =
            component_node.definition.state_var_definitions.iter()
            .map(|(&sv_name, sv_variant)| (sv_name, StateForStateVar::new(&sv_variant)))
            .collect();

        if let Some(CopySource::DynamicElement(_, _, _, _)) = component_node.copy_source {
            state_for_this_component.insert(PROP_INDEX_SV, StateForStateVar::new(
                &StateVarVariant::Number(StateVarDefinition::default())
            ));
        }
        component_states.insert(
            component_name.clone(),
            state_for_this_component,
        );
    }
    component_states
}





fn resolve_state_variable(
    core: &DoenetCore,
    component: &ComponentName,
    map: &Instance,
    state_var_ref: &StateRef,
) -> Option<StateVarValue> {

    if let StateRef::ArrayElement(sv_name, _) = state_var_ref {
        resolve_state_variable(core, component, map, &StateRef::SizeOf(sv_name));
    }

    let state_vars = core.component_states.get(component).unwrap();

    // No need to continue if the state var is already resolved or if the element does not exist
    let current_state = state_vars.get(state_var_ref.name())
        .expect(&format!("Component {} has no state var '{:?}'", component, state_var_ref))
        .get_single_state(&state_var_ref.index(), map)
        .expect(&format!("Error accessing state of {}:{:?}", component, state_var_ref));
    if let Some(State::Resolved(current_value)) = current_state {
        return Some(current_value);
    } else if current_state.is_none() {
        // There is nothing to resolve
        log_debug!("{}:{} does not exist", component, state_var_ref);
        return None
    }

    log_debug!("Resolving {}:{} ({:?})", component, state_var_ref, map);

    let my_dependencies = dependencies_of_state_var(&core.dependencies, component, state_var_ref);
    log_debug!("Dependencies of {}:{} {:?}", component, state_var_ref, my_dependencies);

    let mut dependency_values: HashMap<InstructionName, Vec<DependencyValue>> = HashMap::new();
    for (dep_name, deps) in my_dependencies {
        let mut values_for_this_dep: Vec<DependencyValue> = Vec::new();

        for dep in deps {
            let dependency_source = get_source_for_dependency(core, &dep, &core.essential_data);

            match dep {
                Dependency::StateVar { component_group, instance, state_var_slice } => {

                    let dependency_instances = apply_relative_instance(core, component, map, &component_group.name(), &instance);
                    // log_debug!("instance {:?} for group {:?}", dependency_map, component_group);

                    for dep_instance in dependency_instances {
                        for component_ref in component_group_members(core, component_group.clone(), dep_instance.clone()) {

                            // log_debug!("Component ref {:?} and {:?}", component_ref, state_var_slice);
                            let (sv_comp, comp_map, sv_slice) = convert_component_ref_state_var(
                                core,
                                component_ref,
                                dep_instance.clone(),
                                state_var_slice.clone()
                            ).unwrap();
                            // log_debug!("Converted to sv_comp {} and sv_slice {}", sv_comp, sv_slice);

                            values_for_this_dep.extend(
                                get_dependency_values_for_state_var_slice(core, &sv_comp, &comp_map, &sv_slice)
                            );
                        }
                    }

                },

                Dependency::UndeterminedChildren { component_name , desired_profiles } => {

                    let group_member = component_group_members(core, ComponentGroup::Collection(component_name.clone()), map.clone());

                    let mut children = Vec::new();
                    for component_ref in group_member {
                        children.extend(state_vars_for_undetermined_children(core, map, &component_ref, &desired_profiles));
                    }
                    for (child, sv_slice, dep_source, comp_map) in children {
                        match child {
                            ObjectName::Component(sv_comp) => {

                                values_for_this_dep.extend(
                                    get_dependency_values_for_state_var_slice(core, &sv_comp, &comp_map, &sv_slice.unwrap())
                                );

                            },
                            ObjectName::String(s) => {
                                values_for_this_dep.push(DependencyValue {
                                    source: dep_source,
                                    value: StateVarValue::String(s),
                                })
                            },
                        };
                    }

                },

                Dependency::MapSources { map_sources, state_var_slice } => {

                    let member = map_sources_dependency_member(core, component, map, &map_sources);

                    if let Some((component_ref, comp_map)) = member {
                        log_debug!("map source ref: {:?} map{:?}", component_ref, comp_map);

                        let (sv_comp, comp_map, sv_slice) = convert_component_ref_state_var(core, component_ref, comp_map, state_var_slice.clone()).unwrap();

                        values_for_this_dep.extend(
                            get_dependency_values_for_state_var_slice(core, &sv_comp, &comp_map, &sv_slice)
                        );
                    }
                },
                Dependency::StateVarArrayCorrespondingElement { component_group , array_state_var_name } => {

                    let dependency_instances = apply_relative_instance(core, component, map, &component_group.name(), &RelativeInstance::default());

                    for dep_instance in dependency_instances {
                        for component_ref in component_group_members(core, component_group.clone(), dep_instance.clone()) {
                            let sv_ref = StateRef::from_name_and_index(array_state_var_name, state_var_ref.index());
                            let sv_slice = StateVarSlice::Single(sv_ref);
                            let (sv_comp, comp_map, sv_slice) = convert_component_ref_state_var(core, component_ref, dep_instance.clone(), sv_slice).unwrap();

                            values_for_this_dep.extend(
                                get_dependency_values_for_state_var_slice(core, &sv_comp, &comp_map, &sv_slice)
                            );
                        }
                    }
                },

                Dependency::Essential { component_name, origin } => {

                    let dependency_map = apply_relative_instance_single(&core.component_nodes, component, map, &component_name, &RelativeInstance::default());

                    let index = match origin {
                        EssentialDataOrigin::StateVar(_) => state_var_ref.index(),
                        _ => StateIndex::Basic,
                    };

                    let value = core.essential_data
                        .get(&component_name).unwrap()
                        .get(&origin).unwrap()
                        .clone()
                        .get_value(index, &dependency_map);
    
                    if let Some(value) = value {
                        values_for_this_dep.push(DependencyValue {
                            source: dependency_source,
                            value,
                        })
                    }
                },

                Dependency::StateVarArrayDynamicElement { component_name, array_state_var_name, index_state_var } => {

                    let dependency_map = apply_relative_instance_single(&core.component_nodes, component, map, &component_name, &RelativeInstance::default());

                    let index_value = resolve_state_variable(
                        core,
                        component, // myself
                        &dependency_map,
                        &index_state_var,
                    );

                    let index: Option<usize> = index_value.and_then(|i|
                        convert_float_to_usize(i.try_into().unwrap())
                    );

                    if let Some(index) = index {

                        log_debug!("got prop index which is {}", index);

                        let sv_slice = StateVarSlice::Single(StateRef::ArrayElement(array_state_var_name, index));

                        values_for_this_dep.extend(
                            get_dependency_values_for_state_var_slice(core, &component_name, &dependency_map, &sv_slice)
                        );
                    }
                }
            }
        }

        dependency_values.insert(dep_name, values_for_this_dep);
    }


    log_debug!("Dependency values for {}:{}: {:#?}", &component, state_var_ref, dependency_values);

    let node = core.component_nodes.get(component).unwrap();

    let update_instruction = generate_update_instruction_for_state_ref(
        node,
        state_var_ref,
        dependency_values
    ).expect(&format!("Can't resolve {}:{} (a {} component type)",
        component, state_var_ref, node.definition.component_type)
    );

    let new_value = handle_update_instruction(component, map, state_var_ref, state_vars, update_instruction);

    return new_value;
}

/// This determines the state var given its dependency values.
fn generate_update_instruction_for_state_ref(
    component: &ComponentNode,
    state_var: &StateRef,
    dependency_values: HashMap<InstructionName, Vec<DependencyValue>>

) -> Result<StateVarUpdateInstruction<StateVarValue>, String> {

    if state_var.name() == PROP_INDEX_SV {
        prop_index_determine_value(dependency_values).map(|update_instruction| match update_instruction {
            StateVarUpdateInstruction::NoChange => StateVarUpdateInstruction::NoChange,
            StateVarUpdateInstruction::SetValue(num_val) => StateVarUpdateInstruction::SetValue(num_val.into()),
        })
    } else {

        let state_var_def = component.definition.state_var_definitions.get(state_var.name()).unwrap();

        match state_var {
            StateRef::Basic(_) => {
                state_var_def.determine_state_var_from_dependencies(dependency_values)
            },
            StateRef::SizeOf(_) => {
                state_var_def.determine_size_from_dependencies(dependency_values)
            },
            StateRef::ArrayElement(_, id) => {
                let internal_id = id - 1;
                state_var_def.determine_element_from_dependencies(internal_id, dependency_values)
            }
        }    
    }

}

/// Sets the state var and returns the new value
fn handle_update_instruction(
    component_name: &ComponentName,
    map: &Instance,
    state_var_ref: &StateRef,
    component_state_vars: &HashMap<StateVarName, StateForStateVar>,
    instruction: StateVarUpdateInstruction<StateVarValue>
) -> Option<StateVarValue> {

    // log_debug!("handling update instruction {:?}", &instruction);

    let state_var = component_state_vars.get(state_var_ref.name()).unwrap();

    let updated_value: Option<StateVarValue>;

    match instruction {
        StateVarUpdateInstruction::NoChange => {
            let current_value = component_state_vars.get(state_var_ref.name()).unwrap()
                .get_single_state(&state_var_ref.index(), map)
                .expect(&format!("Error accessing state of {}:{:?}", component_name, state_var_ref));

            match current_value {
                Some(State::Stale) => 
                    panic!("Cannot use NoChange update instruction on a stale value"),
                Some(State::Resolved(current_resolved_value)) => {
                    // Do nothing. It's resolved, so we can use it as is
                    updated_value = Some(current_resolved_value);
                },
                None => {
                    updated_value = None;
                },
            }
        },
        StateVarUpdateInstruction::SetValue(new_value) => {

            updated_value = state_var.set_single_state(&state_var_ref.index(), new_value, map).unwrap();
            // .expect(&format!("Failed to set {}:{} while handling SetValue update instruction", component.name, state_var_ref)
            // );
        }

    };

    log_debug!("Updated {}_map{:?}:{} to {:?}", component_name, map, state_var_ref, updated_value);

    return updated_value;
}

fn get_dependency_values_for_state_var_slice(
    core: &DoenetCore,
    sv_comp: &ComponentName,
    comp_map: &Instance,
    sv_slice: &StateVarSlice
) -> Vec<DependencyValue> {
    let mut dependency_values = vec![];

    let dependency_source = DependencySource::StateVar {
        component_type: core.component_nodes.get(sv_comp).unwrap().definition.component_type,
        state_var_name: sv_slice.name()
    };

    match sv_slice {
        StateVarSlice::Single(ref sv_ref) => {

            let depends_on_value = resolve_state_variable(core, &sv_comp, &comp_map, sv_ref);

            if let Some(depends_on_value) = depends_on_value {
                dependency_values.push(DependencyValue {
                    source: dependency_source.clone(),
                    value: depends_on_value,
                });    
            }
        }
        StateVarSlice::Array(array_state_var_name) => {

            // important to resolve the size before the elements
            let size_value: usize = resolve_state_variable(
                core, &sv_comp, &comp_map, &StateRef::SizeOf(array_state_var_name))
            .expect("Array size should always resolve to a StateVarValue")
            .try_into()
            .unwrap();
            
            for id in indices_for_size(size_value) {

                let element_value = resolve_state_variable(
                    core,
                    &sv_comp,
                    &comp_map,
                    &StateRef::ArrayElement(array_state_var_name, id)
                );

                if let Some(element_value) = element_value {
                    dependency_values.push(DependencyValue {
                        source: dependency_source.clone(),
                        value: element_value,
                    });

                }

            }
        }
    }
    dependency_values
}





// TODO: Use &Dependency instead of cloning
fn dependencies_of_state_var(
    dependencies: &HashMap<DependencyKey, Vec<Dependency>>,
    component_name: &ComponentName,
    state_ref: &StateRef,
) -> HashMap<InstructionName, Vec<Dependency>> {

    let deps = dependencies.iter().filter_map(| (key, deps) |

        match key {
            DependencyKey::StateVar(comp_name, sv_slice, instruct_name) => {

                // Check if the key is me
                if comp_name == component_name {
                    if sv_slice.as_single() == Some(state_ref) {
                        Some((*instruct_name, deps))

                    } else if let StateRef::ArrayElement(..) = state_ref {

                        // The key might also be an array who feeds into me
                        if let StateVarSlice::Array(array_name) = sv_slice {
                            if state_ref.name() == *array_name {
                                Some((*instruct_name, deps))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            },
        }
    );

    // log_debug!("Deps for {}:{} with possible duplicates {:?}", component_name, state_var_slice, deps.clone().collect::<HashMap<InstructionName, &Vec<Dependency>>>());

    let mut combined: HashMap<InstructionName, Vec<Dependency>> = HashMap::new();
    for (k, v) in deps {
        if let Some(accum) = combined.get_mut(k) {
            let dedup: Vec<Dependency> = v.clone().into_iter().filter(|x| !accum.contains(x)).collect();
            accum.extend(dedup);
        } else {
            combined.insert(k, v.clone());
        }
    }
    
    // log_debug!("Dependencies for {}:{} {:?}", component_name, state_var_slice, combined);

    combined
}


fn get_source_for_dependency(
    core: &DoenetCore,
    dependency: &Dependency,
    essential_data: &HashMap<ComponentName, HashMap<EssentialDataOrigin, EssentialStateVar>>
) -> DependencySource {

    match dependency {
        Dependency::Essential { component_name, origin } => {

                let data = essential_data.get(component_name).unwrap().get(origin).unwrap();

                DependencySource::Essential {
                    value_type: data.get_type_as_str()
                }

        },

        Dependency::StateVarArrayCorrespondingElement { component_group, array_state_var_name } => {
            let component_type = group_member_definition(
                &core.component_nodes,
                component_group,
            ).component_type;

            DependencySource::StateVar {
                component_type,
                state_var_name: array_state_var_name,
            }
        }
        Dependency::StateVar { component_group, state_var_slice, .. } => {
            let component_type = group_member_definition(
                &core.component_nodes,
                component_group,
            ).component_type;

            DependencySource::StateVar {
                component_type,
                state_var_name: state_var_slice.name()
            }
        },
        Dependency::UndeterminedChildren { .. } => {
            DependencySource::StateVar {
                component_type: "undetermined",
                state_var_name: "undetermined",
            }
        },
        Dependency::MapSources { map_sources, state_var_slice } => {
            let component_type = group_member_definition(
                &core.component_nodes,
                &ComponentGroup::Collection(map_sources.clone()),
            ).component_type;

            DependencySource::StateVar {
                component_type,
                state_var_name: state_var_slice.name()
            }
        },

        Dependency::StateVarArrayDynamicElement { component_name, array_state_var_name, .. } => {
            let component_type = core.component_nodes
                .get(component_name).unwrap()
                .definition
                .component_type;
            DependencySource::StateVar {
                component_type,
                state_var_name: &array_state_var_name
            }
        }

    }
}

/// Also includes the values of essential data
fn get_dependency_sources_for_state_var(
    core: &DoenetCore,
    component_name: &ComponentName,
    map: &Instance,
    state_ref: &StateRef,
) -> HashMap<InstructionName, Vec<(DependencySource, Option<StateVarValue>)>> {
    
    let my_dependencies = dependencies_of_state_var(&core.dependencies, component_name, state_ref);
    let mut dependency_sources: HashMap<InstructionName, Vec<(DependencySource, Option<StateVarValue>)>> = HashMap::new();

    for (instruction_name, dependencies) in my_dependencies {
        let instruction_sources: Vec<(DependencySource, Option<StateVarValue>)> = dependencies.iter().map(|dependency| {
            let source = get_source_for_dependency(core, &dependency, &core.essential_data);

            let essential_value = if let Dependency::Essential { origin, .. } = dependency {
                let data = core.essential_data
                    .get(component_name).unwrap()
                    .get(origin).unwrap();
                let value = data.get_value(state_ref.index(), map).unwrap();
                Some(value)

            } else {
                None
            };

            (source, essential_value)
        }).collect();

        dependency_sources.insert(instruction_name, instruction_sources);
    }

    dependency_sources
}

/// This must resolve the size
fn elements_of_array(
    core: &DoenetCore,
    component: &ComponentName,
    map: &Instance,
    array_name: &StateVarName,
) -> Vec<StateRef> {
    let size_ref = StateRef::SizeOf(array_name);
    let size: usize = resolve_state_variable(core, component, map, &size_ref)
        .unwrap()
        .try_into()
        .unwrap();

    indices_for_size(size).map(|i| StateRef::ArrayElement(array_name, i)).collect()
}

fn state_vars_for_undetermined_children(
    core: &DoenetCore,
    map: &Instance,
    component_ref: &ComponentRef,
    desired_profiles: &Vec<ComponentProfile>,
) -> Vec<(ObjectName, Option<StateVarSlice>, DependencySource, Instance)> {
    let mut source_and_value = vec![];

    for (member_child, comp_map, _) in get_children_and_members(core, &component_ref, &map) {
        match member_child {
            ObjectRefName::Component(child_ref) => {

                let child_def = component_ref_definition(&core.component_nodes, &child_ref);

                match  &child_def.replacement_components {
                    Some(ReplacementComponents::Children) => {
                        source_and_value.extend(state_vars_for_undetermined_children(core, &comp_map, &child_ref, desired_profiles));
                        continue;
                    }
                    _ => (),
                };
                        
                if let Some(relevant_sv) = child_def.component_profile_match(&desired_profiles) {
                    let (sv_comp, comp_map, sv_slice) = convert_component_ref_state_var(core, child_ref, comp_map, relevant_sv).unwrap();

                    let dependency_source = DependencySource::StateVar {
                        component_type: child_def.component_type,
                        state_var_name: sv_slice.name()
                    };
                    source_and_value.push((ObjectName::Component(sv_comp), Some(sv_slice), dependency_source, comp_map));
                }
            },
            ObjectRefName::String(s) => {
                let dependency_source = DependencySource::StateVar {
                    component_type: "string",
                    state_var_name: "",
                };
                source_and_value.push((ObjectName::String(s), None, dependency_source, comp_map));
            },
        };
    }
    source_and_value
}






fn mark_stale_state_var_and_dependencies(
    core: &DoenetCore,
    component: &ComponentName,
    map: &InstanceGroup,
    state_var_slice: &StateVarSlice,
) {
    let state = core.component_states.get(component).unwrap()
        .get(state_var_slice.name()).unwrap();

    log_debug!("Check stale {}_map{:?}:{}", component, map, state_var_slice);
    for instance in state.instances_where_slice_is_resolved(state_var_slice, map) {

        let instance = instance
            .expect(&format!("Error accessing state of {}:{:?}", component, state_var_slice));
        log_debug!("Marking stale {}_map{:?}:{}", component, instance, state_var_slice);

        state.mark_stale_slice(state_var_slice, &instance);

        let depending_on_me = get_state_variables_depending_on_me(core, component, &instance, state_var_slice);

        for (depending_comp, depending_map, depending_slice) in depending_on_me {
            mark_stale_state_var_and_dependencies(core, &depending_comp, &depending_map, &depending_slice);
        }
    }
}

fn mark_stale_essential_datum_dependencies(
    core: &DoenetCore,
    component_name: ComponentName,
    map: &Instance,
    state_index: &StateIndex,
    origin: EssentialDataOrigin,
) {

    // log_debug!("Marking stale essential {}:{}", component_name, state_var);

    let search_dep = Dependency::Essential {
        component_name,
        origin,
    };

    let my_dependencies = core.dependencies.iter().filter_map( |(key, deps) | {
        if deps.contains(&search_dep) {

            match key {
                DependencyKey::StateVar(comp_name, StateVarSlice::Single(s), _) => Some((comp_name, s.clone())),
                DependencyKey::StateVar(comp_name, StateVarSlice::Array(array), _) => {

                    match state_index {
                        StateIndex::Element(i) =>
                            Some((comp_name, StateRef::ArrayElement(array, *i))),
                        StateIndex::SizeOf =>
                            Some((comp_name, StateRef::SizeOf(array))),
                        StateIndex::Basic =>
                            // Arrays cannot use essential data
                            // associated with a basic state var.
                            None,
                    }
                }
            }

        } else {
            None
        }
    });

    for (component_name, state_var_ref) in my_dependencies {
        mark_stale_state_var_and_dependencies(
            core,
            &component_name,
            map,
            &StateVarSlice::Single(state_var_ref),
        );
    }
}

/// Calculate all the state vars that depend on the given state var
fn get_state_variables_depending_on_me(
    core: &DoenetCore,
    sv_component: &ComponentName,
    map: &Instance,
    sv_slice: &StateVarSlice,
) -> Vec<(ComponentName, InstanceGroup, StateVarSlice)> {

    let mut depending_on_me = vec![];

    for (dependency_key, dependencies) in core.dependencies.iter() {
        for dependency in dependencies {

            match dependency {
                Dependency::StateVar { component_group, instance, state_var_slice } => {
                    let slice_depends = match component_group {
                        ComponentGroup::Single(ComponentRef::BatchMember(n, b, i)) => {
                            let state_var_slice = batch_state_var(core, &n, *b, state_var_slice.clone(), *i).unwrap();
                            slice_depends_on_slice(&state_var_slice, sv_slice)
                        },
                        ComponentGroup::Batch(n) => {
                            // TODO: check if any index depends on sv_slice, not just some range
                            (1..4)
                                .filter_map(|i| batch_state_var(core, &n, None, state_var_slice.clone(), i))
                                .any(|s| slice_depends_on_slice(&s, sv_slice))
                        },
                        _ => slice_depends_on_slice(state_var_slice, sv_slice),
                    };

                    if group_includes_component(&core.group_dependencies, component_group, sv_component)
                    && slice_depends {

                        let DependencyKey::StateVar(dependent_comp, dependent_slice, _) = dependency_key;
                        let instance_group = instance_relative_to_group(core, sv_component, map, dependent_comp, instance);
                        depending_on_me.push(
                            (dependent_comp.clone(), instance_group, dependent_slice.clone())
                        );
                    }
                },

                Dependency::UndeterminedChildren { component_name, desired_profiles } => {
                    let sv_component_def = core.component_nodes.get(sv_component).unwrap().definition;
                    let depends =
                        if collection_may_contain(&core.group_dependencies.get(component_name).unwrap(), sv_component) {
                            true
                        } else if sv_component_def.component_profile_match(desired_profiles).is_some() {
                            let mut chain = parent_chain(&core.component_nodes, component_name);
                            let parent = chain.find(|p| {
                                let node_def = &core.component_nodes.get(p).unwrap().definition;
                                !matches!(node_def.replacement_components, Some(ReplacementComponents::Children))
                            });
                            parent.as_ref() == Some(component_name)
                        } else {
                            false
                        };
                    if depends {
                        let DependencyKey::StateVar(dependent_comp, dependent_slice, _) = dependency_key;
                        let instance_group = instance_relative_to_group(
                            core, sv_component, map, dependent_comp, &RelativeInstance::default());
                        depending_on_me.push(
                            (dependent_comp.clone(), instance_group, dependent_slice.clone())
                        );
                    }
                },

                Dependency::MapSources { map_sources, state_var_slice } => {
                    if sv_component == map_sources
                    && slice_depends_on_slice(state_var_slice, sv_slice) {

                        let DependencyKey::StateVar(dependent_comp, dependent_slice, _) = dependency_key;
                        let instance_group = instance_relative_to_group(
                            core, sv_component, map, dependent_comp, &RelativeInstance::default());
                        depending_on_me.push(
                            (dependent_comp.clone(), instance_group, dependent_slice.clone())
                        );
                    }
                },

                Dependency::StateVarArrayCorrespondingElement { component_group, array_state_var_name } => {
                    if group_includes_component(&core.group_dependencies, component_group, sv_component)
                    && *array_state_var_name == sv_slice.name() {

                        let dependent_slice = sv_slice;
                        let DependencyKey::StateVar(dependent_comp, _, _) = dependency_key;
                        let instance_group = instance_relative_to_group(
                            core, sv_component, map, dependent_comp, &RelativeInstance::default());
                        depending_on_me.push(
                            (dependent_comp.clone(), instance_group, dependent_slice.clone())
                        );
                    }
                },

                Dependency::StateVarArrayDynamicElement {
                    component_name,
                    array_state_var_name,
                    ..
                } => {

                    let this_array_refers_to_me = 
                        component_name == sv_component
                        && *array_state_var_name == sv_slice.name();

                    let i_am_prop_index_of_this_dependency = 
                        // The key that this dependency is under is myself
                        // Aka, the index is supposed to be in my component, not another component
                        dependency_key.component_name() == sv_component
                        // I am actually a propIndex, and not some other state var
                        && sv_slice == &StateVarSlice::Single(StateRef::Basic("propIndex"));

                    if this_array_refers_to_me || i_am_prop_index_of_this_dependency {

                        let DependencyKey::StateVar(dependent_comp, dependent_slice, _) = dependency_key;
                        let instance_group = instance_relative_to_group(
                            core, sv_component, map, dependent_comp, &RelativeInstance::default());
                        depending_on_me.push(
                            (dependent_comp.clone(), instance_group, dependent_slice.clone())
                        );
                    }
                },

                // Essential dependencies are endpoints
                Dependency::Essential { .. } => {},

            }
        }
    }

    fn group_includes_component(
        group_dependencies: &HashMap<ComponentName, Vec<CollectionMembers>>,
        group: &ComponentGroup,
        component: &ComponentName,
    ) -> bool {
        match group {
            ComponentGroup::Single(ComponentRef::Basic(n)) |
            ComponentGroup::Batch(n) |
            ComponentGroup::Single(ComponentRef::BatchMember(n, _, _)) =>
                n == component,
            ComponentGroup::Single(ComponentRef::CollectionMember(n, _)) |
            ComponentGroup::Collection(n) =>
                collection_may_contain(group_dependencies.get(n).unwrap(), component)
        }
    }

    fn slice_depends_on_slice(a: &StateVarSlice, b: &StateVarSlice) -> bool {
        use StateVarSlice::*;
       a.name() == b.name()
       && match (a,b) {
           (Array(_), _) |
           (_, Array(_)) |
           (_, Single(StateRef::SizeOf(_))) |
           (Single(StateRef::Basic(_)), Single(StateRef::Basic(_))) =>
                true,
           (Single(StateRef::ArrayElement(i, _)), Single(StateRef::ArrayElement(j, _))) =>
               i == j,
           (_, _) => false
       }
    }

    depending_on_me
}





#[derive(Debug)]
struct RenderedComponent {
    component_ref: ComponentRef,
    map: Instance,
    child_of_copy: Option<ComponentName>,
}

pub fn update_renderers(core: &DoenetCore) -> String {
    let json_obj = generate_render_tree(core);

    log_json!("Component tree after renderer update", utils::json_components(&core.component_nodes, &core.component_states));

    log_json!("Essential data after renderer update",
    utils::json_essential_data(&core.essential_data));

    serde_json::to_string(&json_obj).unwrap()
}

fn generate_render_tree(core: &DoenetCore) -> serde_json::Value {

    let root_node = core.component_nodes.get(&core.root_component_name).unwrap();
    let root_comp_rendered = RenderedComponent {
        component_ref: ComponentRef::Basic(root_node.name.clone()),
        map: Instance::default(),
        child_of_copy: None
    };
    let mut json_obj: Vec<serde_json::Value> = vec![];

    generate_render_tree_internal(core, root_comp_rendered, &mut json_obj);

    serde_json::Value::Array(json_obj)
}

fn generate_render_tree_internal(
    core: &DoenetCore,
    component: RenderedComponent,
    json_obj: &mut Vec<serde_json::Value>,
) {
    use serde_json::{Map, Value, json};

    let component_name = component.component_ref.name().clone();

    log_debug!("generating render tree for {:?}", component);

    let component_definition = component_ref_definition(
        &core.component_nodes,
        &component.component_ref,
    );

    let renderered_state_vars = component_definition
        .state_var_definitions
        .into_iter()
        .filter_map(|(k, v)| {
            if v.for_renderer() {
                match v.is_array() {
                    true => Some(StateVarSlice::Array(k)),
                    false => Some(StateVarSlice::Single(StateRef::Basic(k))),
                }
            } else {
                None
            }
        });

    let state_var_aliases = match &component_definition.renderer_type {
        RendererType::Special { state_var_aliases, .. } => state_var_aliases.clone(),
        RendererType::Myself => HashMap::new(),
    };

    let mut state_values = serde_json::Map::new();
    for state_var_slice in renderered_state_vars {
        let (sv_comp, sv_map, sv_slice) = convert_component_ref_state_var(
            core,
            component.component_ref.clone(),
            component.map.clone(),
            state_var_slice
        ).unwrap();

        match sv_slice {
            StateVarSlice::Array(sv_name) => {

                let sv_refs = elements_of_array(core, &component_name, &sv_map, &sv_name);

                let mut values: Vec<f64> = Vec::new();
                for sv_ref in sv_refs {
                    values.push(
                        resolve_state_variable(core, &sv_comp, &sv_map, &sv_ref)
                        .unwrap()
                        .try_into()
                        .unwrap()
                    );
                }

                let sv_renderer_name = state_var_aliases
                    .get(&sv_slice.name())
                    .map(|x| *x)
                    .unwrap_or(sv_slice.name())
                    .to_string();

                // hardcoded exceptions
                if sv_renderer_name == "numericalPoints" {
                    let array_2d = [[values[0], values[1]], [values[2], values[3]]];
                    state_values.insert(sv_renderer_name, json!(array_2d));
                } else {
                    state_values.insert(sv_renderer_name, json!(values));
                }
            },

            StateVarSlice::Single(state_ref) => {
                let state_var_value = resolve_state_variable(core, &sv_comp, &sv_map, &state_ref)
                    .expect(&format!("state var {}:{} cannot be resolved", &sv_comp, &state_ref));

                let sv_renderer_name = state_var_aliases
                    .get(&state_ref.name())
                    .map(|x| *x)
                    .unwrap_or(state_ref.name())
                    .to_string();

                // hardcoded exceptions:
                if sv_renderer_name == "selectedStyle" || sv_renderer_name == "graphicalDescendants" {
                    if let StateVarValue::String(v) = state_var_value {
                        // log_debug!("deserializing for renderer: {}", v);
                        let value = serde_json::from_str(&v).unwrap();
                        state_values.insert(sv_renderer_name, value);
                    }
                } else {
                    state_values.insert(sv_renderer_name, state_var_value.into());
                }
            },
        }
    }

    let name_to_render = name_rendered_component(&component, component_definition.component_type);

    let mut children_instructions = Vec::new();
    let node = core.component_nodes.get(&component_name).unwrap();
    if component_definition.should_render_children {
        for (child, child_map, actual_parent) in get_children_and_members(core, &component.component_ref, &component.map) {
            match child {
                ObjectRefName::Component(comp_ref) => {
                    // recurse for children

                    let child_component = RenderedComponent {
                        component_ref: comp_ref,
                        map: child_map.clone(),
                        child_of_copy: component.child_of_copy.clone().or(
                            if std::ptr::eq(actual_parent, node) {
                                None
                            } else {
                                Some(component_name.clone())
                            }
                        ),
                    };

                    let child_definition =
                        component_ref_definition(&core.component_nodes, &child_component.component_ref);

                    let child_name = name_rendered_component(&child_component, child_definition.component_type);

                    let (component_original, child_map) =
                        component_ref_original_component(core, child_component.component_ref.clone(), child_map);

                    let action_component_name = alias_name_with_map_instance(
                        &component_original,
                        child_map
                    );

                    let child_actions: Map<String, Value> =
                        (child_definition.action_names)()
                        .iter()
                        .map(|action_name| 
                            (action_name.to_string(), json!({
                                "actionName": action_name,
                                "componentName": action_component_name,
                            }))
                        ).collect();

                    let renderer_type = match &child_definition.renderer_type {
                        RendererType::Special{ component_type, .. } => *component_type,
                        RendererType::Myself => child_definition.component_type,
                    };

                    children_instructions.push(json!({
                        "actions": child_actions,
                        "componentName": child_name,
                        "componentType": child_definition.component_type,
                        "effectiveName": child_name,
                        "rendererType": renderer_type,
                    }));

                    generate_render_tree_internal(core, child_component, json_obj); 
                },
                ObjectRefName::String(string) => {
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

fn name_rendered_component(component: &RenderedComponent, component_type: &str) -> String {
    let name_to_render = match &component.component_ref {
        ComponentRef::CollectionMember(n, i) |
        ComponentRef::BatchMember(n, _, i) =>
            format!("__{}_from_({}[{}])", component_type, n, *i),
        _ => component.component_ref.name().clone(),
    };
    let name_to_render = match &component.child_of_copy {
        Some(copy_name) => format!("__cp:{}({})", name_to_render, copy_name),
        None => name_to_render,
    };
    let name_to_render = if component.map.len() == 0 {
            name_to_render
        } else {
            format!("__{}_map{:?}", name_to_render, component.map)
        };
    name_to_render
}

/// Used for action names
fn alias_name_with_map_instance(name: &ComponentName, map: Instance) -> String {
    if map.len() > 0 {
        format!("{:?}{}", map, name)
    } else {
        name.to_string()
    }
}

fn dealias_name_with_map_instance(alias: &String) -> (String, Instance) {
    let chars: Vec<char> = alias.chars().collect();
    if chars[0] == '[' {
        // split map and name parts of alias
        let map_end = chars.iter().position(|&c| c == ']').unwrap();
        let map_chars: String = chars[1..map_end].iter().collect(); // no square brackets
        let name: String = chars[map_end+1..].iter().collect();

        let instance: Vec<usize> = map_chars
            .split(", ")
            .map(|s| s.parse().unwrap() )
            .collect();
        (name, instance)
    } else {
        (alias.clone(), Instance::default())
    }
}





#[derive(Debug)]
pub struct Action {
    pub component_name: ComponentName,
    pub action_name: String,

    /// The keys are not state variable names.
    /// They are whatever name the renderer calls the new value.
    pub args: HashMap<String, Vec<StateVarValue>>,
}

/// Internal structure used to track changes
#[derive(Debug, Clone)]
enum UpdateRequest {
    SetEssentialValue(ComponentName, Instance, EssentialDataOrigin, StateIndex, StateVarValue),
    SetStateVar(ComponentName, Instance, StateRef, StateVarValue),
}

pub fn handle_action_from_json(core: &DoenetCore, action: &str) -> String {

    let (action, action_id) = parse_json::parse_action_from_json(action)
        .expect(&format!("Error parsing json action: {}", action));

    handle_action(core, action);

    action_id
}

pub fn handle_action(core: &DoenetCore, action: Action) {

    log_debug!("Handling action {:#?}", action);

    let (component_name, map) = dealias_name_with_map_instance(&action.component_name);

    let component = core.component_nodes.get(&component_name)
        .expect(&format!("{} doesn't exist, but action {} uses it", action.component_name, action.action_name));

    let state_var_resolver = | state_var_ref | {
        resolve_state_variable(core, &component_name, &map, state_var_ref)
    };

    let state_vars_to_update = (component.definition.on_action)(
        &action.action_name,
        action.args,
        &state_var_resolver,
    );

    for (state_var_ref, requested_value) in state_vars_to_update {

        let request = UpdateRequest::SetStateVar(
            component_name.clone(),
            map.clone(),
            state_var_ref.clone(),
            requested_value,
        );
        process_update_request(core, &request);
    }

    // log_json!("Component tree after action", utils::json_components(&core.component_nodes, &core.component_states));
}


/// Convert the results of `request_dependencies_to_update_value`
/// into UpdateRequest struct.
fn convert_dependency_values_to_update_request(
    core: &DoenetCore,
    component: &ComponentNode,
    map: &Instance,
    state_var: &StateRef,
    requests: HashMap<InstructionName, Result<Vec<DependencyValue>, String>>,
) -> Vec<UpdateRequest> {

    let my_dependencies = dependencies_of_state_var(
        &core.dependencies,
        &component.name,
        state_var
    );

    let mut update_requests = Vec::new();

    for (instruction_name, instruction_requests) in requests {

        let valid_requests = match instruction_requests {
            Err(_e) => {
                log_debug!("Inverse definition for {}:{} failed with: {}", component.name, state_var, _e);
                break;
            },
            Ok(result) => result,
        };

        // stores (group name, index)
        let mut group_index = (None, 0);
        let increment = |group_index: (Option<ComponentName>, usize), n: &ComponentName| {
            if group_index.0 == Some(n.clone()) {
                (Some(n.clone()), group_index.1 + 1)
            } else {
                (Some(n.clone()), 1)
            }
        };


        let instruct_dependencies = my_dependencies.get(instruction_name).expect(
            &format!("{}:{} has the wrong instruction name to determine dependencies",
                component.definition.component_type, state_var)
        );

        assert_eq!(valid_requests.len(), instruct_dependencies.len());

        for (request, dependency) in valid_requests.into_iter().zip(instruct_dependencies.iter()) {

            match dependency {
                Dependency::Essential { component_name, origin } => {
                    update_requests.push(UpdateRequest::SetEssentialValue(
                        component_name.clone(),
                        map.clone(),
                        origin.clone(),
                        state_var.index(),
                        request.value.clone(),
                    ))
                },
                Dependency::StateVar { component_group, instance: _, state_var_slice } => {
                    // TODO: recieving multiple dependencies because of multiple instances
                    let component_ref = match component_group {
                        ComponentGroup::Batch(n) => {
                            group_index = increment(group_index, n);
                            ComponentRef::BatchMember(n.clone(), None, group_index.1 - 1)
                        },
                        ComponentGroup::Collection(n) => {
                            group_index = increment(group_index, n);
                            ComponentRef::CollectionMember(n.clone(), group_index.1 - 1)
                        },
                        ComponentGroup::Single(comp_ref) => {
                            comp_ref.clone()
                        },
                    };

                    if let Some((sv_comp, sv_map, sv_slice)) = convert_component_ref_state_var(core, component_ref, map.clone(), state_var_slice.clone()) {
                        if let StateVarSlice::Single(state_var_ref) = sv_slice {
                            update_requests.push(UpdateRequest::SetStateVar(
                                sv_comp,
                                sv_map,
                                state_var_ref,
                                request.value.clone(),
                            ))
                        }
                    }
                },
                _ => (),
            }
        }

    }

    update_requests

}

fn process_update_request(
    core: &DoenetCore,
    update_request: &UpdateRequest
) {

    log_debug!("Processing update request {:?}", update_request);

    match update_request {
        UpdateRequest::SetEssentialValue(component_ref, map, origin, state_index, requested_value) => {

            let essential_var = core.essential_data
                .get(component_ref).unwrap()
                .get(origin).unwrap();

            essential_var.set_value(
                    state_index.clone(),
                    requested_value.clone(),
                    &map,
                ).expect(
                    &format!("Failed to set essential value for {}, {:?}, {:?}", component_ref, origin, state_index)
                );

            // log_debug!("Updated essential data {:?}", core.essential_data);

            mark_stale_essential_datum_dependencies(core, component_ref.clone(), &map, state_index, origin.clone());
        },

        UpdateRequest::SetStateVar(component_name, map, state_var_ref, requested_value) => {

            let dep_comp = core.component_nodes.get(component_name).unwrap();

            let dep_update_requests = request_dependencies_to_update_value_including_shadow(
                core,
                dep_comp,
                &map,
                state_var_ref,
                requested_value.clone(),
            );

            for dep_update_request in dep_update_requests {
                process_update_request(core, &dep_update_request);
            }

            mark_stale_state_var_and_dependencies(core, component_name, &map, &StateVarSlice::Single(state_var_ref.clone()));
        }
    }
}






/// Converts component group to a vector of component references.
fn component_group_members(
    core: &DoenetCore,
    component_group: ComponentGroup,
    map: Instance,
) -> Vec<ComponentRef> {
    match component_group {
        ComponentGroup::Single(comp_ref) => vec![comp_ref],
        ComponentGroup::Batch(name) =>
            indices_for_size(resolve_batch_size(core, &name, None, &map))
                .map(|i| ComponentRef::BatchMember(name.clone(), None, i)).collect(),
        ComponentGroup::Collection(name) =>
            indices_for_size(collection_size(core, &name, &map))
                .map(|i| ComponentRef::CollectionMember(name.clone(), i)).collect(),
    }
}

fn resolve_batch_size(
    core: &DoenetCore,
    component_name: &ComponentName,
    batch_name: Option<BatchName>,
    map: &Instance,
) -> usize {

    // log_debug!("resolving {} batch {:?} found def ", component_name, batch_name);
    let batch_def = core.component_nodes.get(component_name).unwrap()
        .definition.unwrap_batch_def(&batch_name);
    resolve_state_variable(core, component_name, map, &batch_def.size)
        .unwrap().try_into().unwrap()
}

fn collection_size(
    core: &DoenetCore,
    component_name: &ComponentName,
    map: &Instance,
) -> usize {

    core.group_dependencies.get(component_name).unwrap()
        .iter()
        .map(|c| collection_members_size(core, c, map))
        .sum()
}

fn collection_members_size(
    core: &DoenetCore,
    collection_members: &CollectionMembers,
    map: &Instance,
) -> usize {
    match collection_members {
        CollectionMembers::Component(_) => 1,
        CollectionMembers::Batch(n) => resolve_batch_size(core, n, None, map),
        CollectionMembers::ComponentOnCondition { component_name, condition } =>
            match resolve_state_variable(core, component_name, map, condition) {
                Some(StateVarValue::Boolean(true)) => 1,
                _ => 0,
            },
        CollectionMembers::InstanceBySources { sources, .. } => collection_size(core, sources, map),
    }
}

/// Without resolving any state variables, is it possible that
/// the collection contains the component?
fn collection_may_contain(members: &Vec<CollectionMembers>, component: &ComponentName)
    -> bool {

    members.iter().any(|c|
        match c {
            CollectionMembers::ComponentOnCondition { component_name: n, .. } |
            CollectionMembers::InstanceBySources { template: n, ..} |
            CollectionMembers::Batch(n) |
            CollectionMembers::Component(n) =>
                n == component,
        }
    )
}


// Converts a collection into the ComponentRef it points to.
// Returns ComponentRef of type basic or batch.
fn component_ref_apply_collection(
    core: &DoenetCore,
    component_ref: ComponentRef,
    map: Instance,
) -> Option<(ComponentRef, Instance)> {
    match component_ref {
        ComponentRef::CollectionMember(n, i) => nth_collection_member(core, &n, map, i),
        _ => Some((component_ref, map)),
    }
}

/// Convert (ComponentRef, Instance, StateVarSlice) -> (ComponentName, Instance, StateVarSlice).
/// If the component reference is a group member, these are not the same.
fn convert_component_ref_state_var(
    core: &DoenetCore,
    name: ComponentRef,
    map: Instance,
    state_var: StateVarSlice,
) -> Option<(ComponentName, Instance, StateVarSlice)> {
    let (name, map) = match component_ref_apply_collection(core, name, map) {
        Some((c,m)) => (c, m),
        None => return None,
    };

    match name {
        ComponentRef::Basic(n) => Some((n, map, state_var)),
        ComponentRef::BatchMember(n, c, i) => batch_state_var(core, &n, c, state_var, i)
            .map(|sv| (n, map, sv)),
        _ => None,
    }
}

fn batch_state_var(
    core: &DoenetCore,
    component_name: &ComponentName,
    batch_name: Option<BatchName>,
    state_var: StateVarSlice,
    index: usize,
) -> Option<StateVarSlice> {

    let batch_def = core.component_nodes.get(component_name).unwrap()
        .definition.unwrap_batch_def(&batch_name);
    (batch_def.member_state_var)(index, &state_var)
}

/// The component ref return can either be a basic, or a member of a batch
/// TODO: return is weird
fn nth_collection_member(
    core: &DoenetCore,
    component_name: &ComponentName,
    map: Instance,
    index: usize,
) -> Option<(ComponentRef, Instance)> {

    let mut index = index;
    for c in core.group_dependencies.get(component_name).unwrap() {
        let (size, group_member);
        match c {
            CollectionMembers::Component(component_name) => {
                size = 1;
                group_member = (ComponentRef::Basic(component_name.clone()), map.clone());
            },
            CollectionMembers::Batch(component_name) => {
                size = resolve_batch_size(core, component_name, None, &map);
                group_member = (ComponentRef::BatchMember(component_name.clone(), None, index), map.clone());
            },
            CollectionMembers::ComponentOnCondition { component_name, condition } => {
                let condition = resolve_state_variable(core, component_name, &map, condition);
                size = (condition == Some(StateVarValue::Boolean(true))) as usize;
                group_member = (ComponentRef::Basic(component_name.clone()), map.clone());
            },
            CollectionMembers::InstanceBySources { sources, template } => {
                size = collection_size(core, sources, &map);
                let mut map_new = map.clone();
                map_new.push(index);
                group_member = (ComponentRef::Basic(template.clone()), map_new)
            },
        }
        if index > size {
            index -= size;
        } else {
            return Some(group_member);
        }
    }
    None
}

// Returns the component that the ComponentRef refers to.
// If the ComponentRef refers to a batch member, this returns the entire component.
fn component_ref_original_component(
    core: &DoenetCore,
    component_ref: ComponentRef,
    map: Instance,
) -> (ComponentName, Instance) {
    let (c, m) = component_ref_apply_collection(core, component_ref.clone(), map.clone()).expect(
        &format!("no component original {:?} {:?}", component_ref, map)
    );
    (c.name(), m)
}

fn group_member_definition(
    component_nodes: &HashMap<ComponentName, ComponentNode>,
    component_group: &ComponentGroup,
) -> &'static ComponentDefinition {

    let node = component_nodes.get(&component_group.name()).unwrap();
    node.definition.definition_of_members(&node.static_attributes)
}

fn component_ref_definition(
    component_nodes: &HashMap<ComponentName, ComponentNode>,
    component_ref: &ComponentRef,
) -> &'static ComponentDefinition {

    // log_debug!("Getting component ref definition for {:?}", component_ref);

    let node = component_nodes.get(&component_ref.name()).unwrap();
    match &component_ref {
        ComponentRef::CollectionMember(_, _) =>
            (node.definition.unwrap_collection_def().member_definition)(&node.static_attributes),
        ComponentRef::BatchMember(_, n, _) =>
            node.definition.unwrap_batch_def(n).member_definition,
        ComponentRef::Basic(_) => node.definition,
    }
}





/// An instance is specified with a component to refer to a single map instance.
/// The length is the number of maps a component is inside.
/// Each index selects the map instance the component is part of.
/// Note: instance number starts at 1
pub type Instance = Vec<usize>;

/// When referring to multiple components, the length is shorter than for an Instance.
/// This means it refers to every instance across the omitted indices.
pub type InstanceGroup = Vec<usize>;

/// Similar to InstanceGroup, but it can be used in dependencies of
/// components that are already inside maps.
pub type RelativeInstance = Vec<usize>;

/// Find the component that the sources dependency points to
fn map_sources_dependency_member(
    core: &DoenetCore,
    component: &ComponentName,
    map: &Instance,
    sources: &ComponentName,
) -> Option<(ComponentRef, Instance)> {
    let sources_for_component = sources_that_instance_component(&core.component_nodes, component);
    let sources_index_in_map = sources_for_component.iter().position(|n| n == sources).unwrap();
    let index = *map.get(sources_index_in_map).unwrap();
    let sources_map = map[0..sources_index_in_map].to_vec();
    nth_collection_member(core, sources, sources_map, index)
}

fn instance_relative_to_group(
    core: &DoenetCore,
    component: &ComponentName,
    map: &Instance,
    dependency: &ComponentName,
    relative_map: &RelativeInstance,
) -> InstanceGroup {
    instance_relative_to_group_internal(
        &core.component_nodes,
        component,
        map,
        dependency,
        relative_map
    ).0
}

fn instance_relative_to_group_internal(
    component_nodes: &HashMap<ComponentName, ComponentNode>,
    component: &ComponentName,
    map: &Instance,
    dependency: &ComponentName,
    relative_map: &RelativeInstance,
) -> (InstanceGroup, Vec<ComponentName>) {
    let sources_for_component = sources_that_instance_component(component_nodes, component);
    let sources_for_dependency = sources_that_instance_component(component_nodes, dependency);

    if sources_for_dependency.len() <= sources_for_component.len() {
        // Find the instance of a component inside fewer maps
        let mut instance = Instance::default();
        for i in 0..std::cmp::min(sources_for_dependency.len(), map.len()) {
            assert_eq!(sources_for_dependency.get(i), sources_for_dependency.get(i));
            instance.push(*map.get(i).unwrap());
        }
        (instance, sources_for_dependency)
    } else {
        // Find the instance group of a component in more maps
        let mut combined_map = map.clone();
        combined_map.extend(relative_map);
        (combined_map, sources_for_dependency)
    }
}

fn apply_relative_instance_single(
    component_nodes: &HashMap<ComponentName, ComponentNode>,
    component: &ComponentName,
    map: &Instance,
    dependency: &ComponentName,
    relative_map: &RelativeInstance,
) -> Instance {
    let (group, sources) = instance_relative_to_group_internal(component_nodes, component, map, dependency, relative_map);
    assert_eq!(group.len(), sources.len());
    group
}

fn apply_relative_instance(
    core: &DoenetCore,
    component: &ComponentName,
    map: &Instance,
    dependency: &ComponentName,
    relative_map: &RelativeInstance,
) -> Vec<Instance> {
    let (group, sources) = instance_relative_to_group_internal(&core.component_nodes, component, map, dependency, relative_map);
    all_map_instances(core, &group, &sources[group.len()..])
}

fn all_map_instances(
    core: &DoenetCore,
    map: &Instance,
    sources_remaining: &[ComponentName],
) -> Vec<Instance> {

    if sources_remaining.is_empty() {
        return vec![map.clone()]
    }
    let sources_name = &sources_remaining[0];
    let mut vec = vec![];
    for i in indices_for_size(collection_size(core, &sources_name, map)) {
        let mut next_map = map.clone();
        next_map.push(i);
        vec.extend(all_map_instances(core, &next_map, &sources_remaining[1..]));
    }
    vec
}

fn sources_that_instance_component(
    component_nodes: &HashMap<ComponentName, ComponentNode>,
    component: &ComponentName,
) -> Vec<ComponentName> {
    let parents = parent_chain(component_nodes, component).rev();
    let children = parents.clone().skip(1);
    let mut sources = vec![];
    for (parent, child) in parents.zip(children) {
        let parent = component_nodes.get(&parent).unwrap();
        let child = component_nodes.get(&child).unwrap();
        if parent.definition.component_type == "map"
        && child.definition.component_type == "template" {
            let sources_child = get_children_of_type(component_nodes, parent, "sources", false)
                .next().unwrap().clone();
            sources.push(sources_child.clone());
        }
    }
    sources
}

/// Vector of parents beginning with the immediate parent, ending with the root
fn parent_chain(
    component_nodes: &HashMap<ComponentName, ComponentNode>,
    component: &ComponentName,
) -> impl DoubleEndedIterator<Item=ComponentName> + Clone {
    let mut parent_chain = vec![];
    let mut loop_component = component_nodes.get(component).unwrap();
    while loop_component.parent.is_some() {
        let loop_parent = loop_component.parent.clone().unwrap();
        loop_component = component_nodes.get(&loop_parent).unwrap();
        parent_chain.push(loop_parent);
    }
    parent_chain.into_iter()
}





#[derive(Debug)]
enum ObjectRefName {
    Component(ComponentRef),
    String(String),
}

// return child and actual parent
fn get_children_and_members<'a>(
    core: &'a DoenetCore,
    component: &ComponentRef,
    map: &'a Instance,
) -> impl Iterator<Item=(ObjectRefName, Instance, &'a ComponentNode)> {

    let (use_component_name, use_map) = component_ref_original_component(core, component.clone(), map.clone());
    // log_debug!("use component {} {:?}", use_component_name, use_map);

    get_children_including_copy_and_groups(
        &core,
        core.component_nodes.get(&use_component_name).unwrap(),
        &use_map,
    )
    .into_iter()
    .flat_map(move |(child, actual_parent)| match child {
        ComponentChild::String(s) => vec![(ObjectRefName::String(s.clone()), use_map.clone(), actual_parent)],
        ComponentChild::Component(comp_name) => {

            match &core.component_nodes.get(&comp_name).unwrap().definition.replacement_components {
                Some(ReplacementComponents::Batch(_)) => {
                    let group = ComponentGroup::Batch(comp_name.clone());
                    component_group_members(core, group, use_map.clone()).iter().map(|comp_ref|
                        (ObjectRefName::Component(comp_ref.clone()),
                        use_map.clone(),
                        actual_parent)
                    ).collect::<Vec<(ObjectRefName, Instance, &ComponentNode)>>()
                },
                Some(ReplacementComponents::Collection(_)) => {
                    let group = ComponentGroup::Collection(comp_name.clone());
                    component_group_members(core, group, use_map.clone()).iter().map(|comp_ref|
                        (ObjectRefName::Component(comp_ref.clone()),
                        use_map.clone(),
                        actual_parent)
                    ).collect::<Vec<(ObjectRefName, Instance, &ComponentNode)>>()
                },
                _ => {
                    vec![(ObjectRefName::Component(ComponentRef::Basic(comp_name.clone())),
                    use_map.clone(),
                    actual_parent)]
                }
            }
        },
    })
}

/// An addition to get_children_including_copy that includes copying
/// the component index of a group
fn get_children_including_copy_and_groups<'a>(
    core: &'a DoenetCore,
    component: &'a ComponentNode,
    map: &Instance,
) -> Vec<(ComponentChild, &'a ComponentNode)> {

    let mut children_vec: Vec<(ComponentChild, &ComponentNode)> = Vec::new();
    match &component.copy_source {
        Some(CopySource::Component(component_ref, instance)) => {
            let sources_ref_map = apply_relative_instance_single(&core.component_nodes, &component.name, map, &component_ref.name(), &instance);
            let (source, source_map) = component_ref_original_component(core, component_ref.clone(), sources_ref_map);
            let source_comp = core.component_nodes.get(&source).unwrap();

            children_vec = get_children_including_copy_and_groups(core, source_comp, &source_map);
        },
        Some(CopySource::MapSources(map_sources)) => {
            let (source, source_map) = map_sources_dependency_member(core, &component.name, map, &map_sources).unwrap();
            if let ComponentRef::Basic(name) = source {
                let source_comp = core.component_nodes.get(&name).unwrap();

                children_vec = get_children_including_copy_and_groups(core, source_comp, &source_map);
            }
        },
        _ => {},
    }

    children_vec.extend(
        component.children
        .iter()
        .map(|c| (c.clone(), component))
    );

    children_vec
}


/// This includes the copy source's children.
fn get_children_including_copy<'a>(
    components: &'a HashMap<ComponentName, ComponentNode>,
    component: &'a ComponentNode,
) -> Vec<(&'a ComponentChild, &'a ComponentNode)> {

    // log_debug!("Getting children for {}", component.name);

    let mut children_vec: Vec<(&ComponentChild, &ComponentNode)> = Vec::new();
    if let Some(CopySource::Component(ComponentRef::Basic(ref source), ..)) = component.copy_source {

        let source_comp = components.get(source).unwrap();

        children_vec = get_children_including_copy(components, source_comp);

    // } else if let Some(CopySource::StateVar(_, _)) = component.copy_source {
    //     // If this is a copy prop, add whatever it is copying as a child
    //     children_vec.push((ComponentChild::Component(component.name.clone()), component));
    }

    children_vec.extend(
        component.children
        .iter()
        .map(|c| (c, component))
    );

    children_vec
}


/// Recurse until the name of the original source is found.
/// This allows copies to share essential data.
fn get_recursive_copy_source_component_when_exists<'a>(
    components: &'a HashMap<ComponentName, ComponentNode>,
    component: &'a ComponentNode,
) -> (&'a ComponentName, RelativeInstance) {
    match &component.copy_source {
        Some(CopySource::Component(ComponentRef::Basic(ref source), ref instance1)) => {
            let (component, instance2) = get_recursive_copy_source_component_when_exists(
                components,
                components.get(source).unwrap(),
            );
            (component, instance1.clone().into_iter().chain(instance2).collect())
        },
        _ => (&component.name, RelativeInstance::default()),
    }
}


fn request_dependencies_to_update_value_including_shadow(
    core: &DoenetCore,
    component: &ComponentNode,
    map: &Instance,
    state_var_ref: &StateRef,
    new_value: StateVarValue,
) -> Vec<UpdateRequest> {

    if let Some((source_ref, source_sv_ref)) = state_var_is_shadowing(component, state_var_ref) {

        let (source_comp, sv_map, source_state_var) =
            convert_component_ref_state_var(core, source_ref, map.clone(), source_sv_ref)
            .unwrap();

        let source_state_var = match source_state_var {
            StateVarSlice::Single(sv_ref) => sv_ref,
            StateVarSlice::Array(_) => panic!()
        };
        vec![UpdateRequest::SetStateVar(source_comp, sv_map, source_state_var, new_value)]

    } else {

        let dependency_sources = get_dependency_sources_for_state_var(core, &component.name, map, &state_var_ref.clone());

        log_debug!("Dependency sources for {}:{}, {:?}", component.name, state_var_ref, dependency_sources);

        let requests = component.definition.state_var_definitions.get(state_var_ref.name()).unwrap()
            .request_dependencies_to_update_value(state_var_ref, new_value, dependency_sources)
            .expect(&format!("Failed requesting dependencies for {}:{}", component.name, state_var_ref));

        log_debug!("{}:{} wants its dependency to update to: {:?}", component.name, state_var_ref, requests);

        let update_requests = convert_dependency_values_to_update_request(core, component, map, state_var_ref, requests);

        log_debug!("{}:{} generated update requests: {:#?}", component.name, state_var_ref, update_requests);

        update_requests
    }
}

/// Detect if a state var is shadowing because of a CopySource
/// and has a primary input state variable, which is needed.
fn state_var_is_shadowing(component: &ComponentNode, state_var: &StateRef)
    -> Option<(ComponentRef, StateVarSlice)> {

    if let Some(CopySource::StateVar(ref source_comp, _, ref source_state_var)) = component.copy_source {
        if let Some(primary_input_state_var) = component.definition.primary_input_state_var {

            if state_var == &StateRef::Basic(primary_input_state_var) {
                Some((source_comp.clone(), StateVarSlice::Single(source_state_var.clone())))
            } else {
                None
            }
        } else {
            panic!("{} component type doesn't have a primary input state var", component.definition.component_type);
        }

    } else if let Some(CopySource::DynamicElement(ref source_comp, ref source_state_var, ..)) = component.copy_source {
        if let Some(primary_input_state_var) = component.definition.primary_input_state_var {

            if state_var == &StateRef::Basic(primary_input_state_var) {

                Some((ComponentRef::Basic(source_comp.to_string()), StateVarSlice::Array(source_state_var)))
            } else {
                None
            }
        } else {
            panic!("{} component type doesn't have a primary input state var", component.definition.component_type);
        }


    } else {
        None
    }
}





// ==== Error and warning checks during core creating ====

fn check_for_invalid_childen_component_profiles(component_nodes: &HashMap<ComponentName, ComponentNode>) -> Vec<DoenetMLWarning> {
    let mut doenet_ml_warnings = vec![];
    for (_, component) in component_nodes.iter() {
        if let ValidChildTypes::ValidProfiles(ref valid_profiles) = component.definition.valid_children_profiles {

            for child in component.children.iter().filter_map(|child| child.as_component()) {
                let child_comp = component_nodes.get(child).unwrap();
                let mut has_valid_profile = false;
                let child_member_def = child_comp.definition.definition_of_members(&child_comp.static_attributes);
                for (child_profile, _) in child_member_def.component_profiles.iter() {
                    if valid_profiles.contains(child_profile) {
                        has_valid_profile = true;
                        break;
                    }
                }
                if matches!(child_member_def.replacement_components, Some(ReplacementComponents::Children)) {
                    has_valid_profile = true;
                }

                if has_valid_profile == false {
                    doenet_ml_warnings.push(DoenetMLWarning::InvalidChildType {
                        parent_comp_name: component.name.clone(),
                        child_comp_name: child_comp.name.clone(),
                        child_comp_type: child_member_def.component_type,
                    });
                }
            }
    
        }
    }
    doenet_ml_warnings
}

/// Do this before dependency generation so it doesn't crash
fn check_for_cyclical_copy_sources(component_nodes: &HashMap<ComponentName, ComponentNode>) -> Result<(), DoenetMLError> {
    // All the components that copy another component, along with the name of the component they copy
    let copy_comp_targets: Vec<(&ComponentNode, &ComponentRef)> = component_nodes.iter().filter_map(|(_, c)|
        match c.copy_source {
            Some(CopySource::Component(ref source, _)) => Some((c, source)),
            _ => None,
        }
    ).collect();

    for (copy_component, _) in copy_comp_targets.iter() {
        if let Some(cyclic_error) = check_cyclic_copy_source_component(&component_nodes, copy_component) {
            return Err(cyclic_error);
        }
    }
    return Ok(())
}


fn check_cyclic_copy_source_component(
    components: &HashMap<ComponentName, ComponentNode>,
    component: &ComponentNode,

) -> Option<DoenetMLError> {

    let mut current_comp = component;
    let mut chain = vec![];
    while let Some(CopySource::Component(ref source, _)) = current_comp.copy_source {

        if chain.contains(&current_comp.name) {
            // Cyclical dependency
            chain.push(current_comp.name.clone());

            let start_index = chain.iter().enumerate().find_map(|(index, name)| {
                if name == &current_comp.name {
                    Some(index)
                } else {
                    None
                }
            }).unwrap();

            let (_, relevant_chain) = chain.split_at(start_index);

            return Some(DoenetMLError::CyclicalDependency {
                component_chain: Vec::from(relevant_chain)
            });


        } else {

            chain.push(current_comp.name.clone());
            current_comp = components.get(&source.name()).unwrap();
        }
    }

    None
}


fn check_for_invalid_component_names(
    component_nodes: &HashMap<ComponentName, ComponentNode>,
    component_attributes: &HashMap<ComponentName, HashMap<AttributeName, HashMap<usize, Vec<ObjectName>>>>,
) -> Result<(), DoenetMLError> {

    for attributes_for_comp in component_attributes.values() {
        for attributes in attributes_for_comp.values() {
            for attribute_list in attributes.values() {
                for attr_object in attribute_list {

                    if let ObjectName::Component(comp_obj) = attr_object {
                        if !component_nodes.contains_key(comp_obj) {
                            // The component tried to copy a non-existent component.
                            return Err(DoenetMLError::ComponentDoesNotExist {
                                comp_name: comp_obj.to_owned()
                            });
                        }
                    }
                }
            }
        }
    }
    Ok(())
}


fn check_for_cyclical_dependencies(dependencies: &HashMap<DependencyKey, Vec<Dependency>>) -> Result<(), DoenetMLError> {
   // Now that the dependency graph has been created, use it to check for cyclical dependencies
    // for all the components
    for (dep_key, _) in dependencies.iter() {
        let DependencyKey::StateVar(comp, sv_ref, _) = dep_key;
        let mut chain = vec![(comp.clone(), sv_ref.clone())];
        let possible_error = check_for_cyclical_dependency_chain(&dependencies, &mut chain);

        if let Some(error) = possible_error {
            return Err(error);
        }
    }
    Ok(())
}

/// Check for cyclical dependencies, assuming that we have already traversed through the
/// given dependency chain. This function might become slow for larger documents with lots of copies
fn check_for_cyclical_dependency_chain(
    dependencies: &HashMap<DependencyKey, Vec<Dependency>>,
    dependency_chain: &mut Vec<(ComponentName, StateVarSlice)>,
) -> Option<DoenetMLError> {

    // log_debug!("Dependency chain {:?}", dependency_chain);
    let last_link = dependency_chain.last().unwrap().clone();

    let my_dependencies = dependencies.iter().filter(|(dep_key, _)| {
        let DependencyKey::StateVar(comp, sv_slice, _) = dep_key;
        if comp == &last_link.0 && sv_slice == &last_link.1 {
            true
        } else {
            false
        }
    });

    for (_, dep_list) in my_dependencies {
        for dep in dep_list {
            let new_link = match dep {
                Dependency::StateVar { component_group, state_var_slice, .. } => {
                    Some((component_group.name().clone(), state_var_slice.clone()))
                },
                _ => None,
            };

            if let Some(new_link) = new_link {
                if dependency_chain.contains(&new_link) {
                    // Cyclical dependency!!

                    dependency_chain.push(new_link.clone());
                    log_debug!("Cyclical dependency through {:?} with duplicate {:?}", dependency_chain, new_link);

                    let start_index = dependency_chain.iter().enumerate().find_map(|(index, item)| {
                        if item == &new_link {
                            Some(index)
                        } else {
                            None
                        }
                    }).unwrap();

                    let (_, relevant_chain) = dependency_chain.split_at(start_index);
                    let mut component_chain = vec![];
                    for link in relevant_chain.into_iter() {
                        if component_chain.is_empty() || component_chain.last().unwrap() != &link.0 {
                            component_chain.push(link.0.clone());
                        }
                    }

                    return Some(DoenetMLError::CyclicalDependency {
                        component_chain
                    });

                } else {
                    dependency_chain.push(new_link);
                    let possible_error = check_for_cyclical_dependency_chain(dependencies, dependency_chain);
                    dependency_chain.pop();

                    if let Some(error) = possible_error {
                        return Some(error);
                    }
                }
            }
        }
    }

    None
}



fn convert_float_to_usize(f: f64) -> Option<usize> {
    let my_int = f as i64;
    if my_int as f64 == f {
        // no loss of precision
        usize::try_from(my_int).ok()
    } else {
        None
    }
}

fn indices_for_size(size: usize) -> std::ops::Range<usize> {
    1..size+1
}
