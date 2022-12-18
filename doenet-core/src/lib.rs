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
pub struct DependencyKey (ComponentStateSliceAllInstances, InstructionName);

impl DependencyKey {
    fn component_name(&self) -> &str {
        &self.0.0
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
        component_states: ComponentGroupSliceRelative,
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
        array_state_var: ComponentRefArrayRelative,
    },

    StateVarArrayDynamicElement {
        array_state_var: ComponentRefArrayRelative,
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
    let copy_ref_relative = ComponentRefRelative(comp_ref.clone(), copy_instance);

    let copy_prop = ml_component.copy_prop.as_ref();
    if copy_prop.is_none() {
        if !std::ptr::eq(ml_component.definition, source_def) {
            return Err(DoenetMLError::ComponentCannotCopyOtherType {
                component_name: ml_component.name.clone(),
                component_type: ml_component.definition.component_type,
                source_type: &source_def.component_type,
            });
        }

        return Ok(Some(CopySource::Component(copy_ref_relative)));
    }
    let copy_prop = copy_prop.unwrap();

    if let Some(state_ref) = source_def.array_aliases.get(copy_prop.as_str()) {
        return Ok(Some(CopySource::StateVar(ComponentRefStateRelative(copy_ref_relative, state_ref.clone()))))
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

            Ok(Some(CopySource::StateVar(ComponentRefStateRelative(
                copy_ref_relative,
                StateRef::ArrayElement(source_sv_name, index)
            ))))
        },
        (0, _) => {

            // no index
            if source_sv_def.is_array() {
                return Err(DoenetMLError::CannotCopyArrayStateVar {
                    source_comp_name: comp_ref.name(),
                    source_sv_name,
                });
            }
            Ok(Some(CopySource::StateVar(ComponentRefStateRelative(
                copy_ref_relative,
                StateRef::Basic(source_sv_name)
            ))))
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


fn create_group_dependencies(component_nodes: &HashMap<ComponentName, ComponentNode>)
    -> HashMap<ComponentName, Vec<CollectionMembers>> {

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
            let component_relative = get_recursive_copy_source_component_when_exists(&component_nodes, comp_name);
            component_attributes.get(&component_relative.0)
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

    for component_name in component_nodes.keys() {

        let dependencies_for_this_component = create_all_dependencies_for_component(
            &component_nodes,
            component_name,
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
    component_name: &ComponentName,
    component_attributes: &HashMap<AttributeName, HashMap<usize, Vec<ObjectName>>>,
    // copy_index_flag: Option<&(ComponentName, StateVarName, Vec<ObjectName>)>,
    essential_data: &mut HashMap<ComponentName, HashMap<EssentialDataOrigin, EssentialStateVar>>,
    should_initialize_essential_data: bool,
    element_specific_dependencies: &HashMap<(ComponentRef, StateVarName), Vec<usize>>,
) -> HashMap<DependencyKey, Vec<Dependency>> {

    // log_debug!("Creating dependencies for {}", component.name);
    let component = components.get(component_name).unwrap();
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

            let component_slice = ComponentStateSliceAllInstances(
                component_name.clone(),
                StateVarSlice::Single(StateRef::SizeOf(state_var_name))
            );
            for (instruct_name, ref dep_instruction) in size_dep_instructions.into_iter() {
                let instruct_dependencies = create_dependencies_from_instruction(
                    &components,
                    &component_slice,
                    component_attributes,
                    dep_instruction,
                    instruct_name,
                    essential_data,
                    should_initialize_essential_data,
                );

                dependencies.insert(
                    DependencyKey(component_slice.clone(), instruct_name),
                    instruct_dependencies,
                );

            }

            let array_dep_instructions = state_var_variant
                .return_array_dependency_instructions(HashMap::new());

            let component_slice = ComponentStateSliceAllInstances(
                component_name.clone(),
                StateVarSlice::Array(state_var_name),
            );
            for (instruct_name, ref dep_instruction) in array_dep_instructions.into_iter() {
                let instruct_dependencies =
                    create_dependencies_from_instruction(
                        &components,
                        &component_slice,
                        component_attributes,
                        dep_instruction,
                        instruct_name,
                        essential_data,
                        should_initialize_essential_data
                    );

                dependencies.insert(
                    DependencyKey(component_slice.clone(), instruct_name),
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

                let component_slice = ComponentStateSliceAllInstances(
                    component_name.clone(),
                    StateVarSlice::Single(StateRef::ArrayElement(state_var_name, index)),
                );
                for (instruct_name, ref dep_instruction) in element_dep_instructions.into_iter() {
                    let instruct_dependencies =
                        create_dependencies_from_instruction(
                            &components,
                            &component_slice,
                            component_attributes,
                            dep_instruction,
                            instruct_name,
                            essential_data,
                            should_initialize_essential_data
                        );

                    dependencies.insert(
                        DependencyKey(component_slice.clone(), instruct_name),
                        instruct_dependencies,
                    );
                }
            }


        } else {

            let dependency_instructions = state_var_variant.return_dependency_instructions(HashMap::new());

            let component_slice = ComponentStateSliceAllInstances(
                component_name.clone(),
                StateVarSlice::Single(StateRef::Basic(state_var_name)),
            );
            for (instruct_name, ref dep_instruction) in dependency_instructions.into_iter() {
                let instruct_dependencies = create_dependencies_from_instruction(
                    &components,
                    &component_slice,
                    component_attributes,
                    dep_instruction,
                    instruct_name,
                    essential_data,
                    should_initialize_essential_data
                );

                dependencies.insert(
                    DependencyKey(component_slice.clone(), instruct_name),
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
    component_slice: &ComponentStateSliceAllInstances,
    component_attributes: &HashMap<AttributeName, HashMap<usize, Vec<ObjectName>>>,
    instruction: &DependencyInstruction,
    instruction_name: InstructionName,
    essential_data: &mut HashMap<ComponentName, HashMap<EssentialDataOrigin, EssentialStateVar>>,
    should_initialize_essential_data: bool,
) -> Vec<Dependency> {

    log_debug!("Creating dependency {}:{}:{} from instruction {:?}", component.name, state_var_slice, instruction_name, instruction);

    let component = components.get(&component_slice.0).unwrap();
    let state_var_slice = &component_slice.1;

    match &instruction {

        DependencyInstruction::Essential { prefill } => {

            let source_relative = get_recursive_copy_source_component_when_exists(components, &component_slice.0);
            let essential_origin = EssentialDataOrigin::StateVar(state_var_slice.name());

            if should_initialize_essential_data && source_relative.0 == component.name {
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
                    &source_relative.0,
                    essential_origin.clone(),
                    initial_data,
                    essential_data
                );
            }

            vec![Dependency::Essential {
                component_name: source_relative.0.clone(),
                origin: essential_origin,
            }]
        },

        DependencyInstruction::StateVar { component_ref, state_var } => {

            let component_ref = match component_ref {
                Some(name) => name.clone(),
                None => ComponentRef::Basic(component.name.clone()),
            };

            let component_states = ComponentGroupSliceRelative(ComponentGroup::Single(component_ref).into(), state_var.clone());
            vec![Dependency::StateVar { component_states }]
        },

        DependencyInstruction::CorrespondingElements { component_ref, array_state_var_name } => {

            let component_ref = match component_ref {
                Some(name) => name.clone(),
                None => ComponentRef::Basic(component.name.clone()),
            };

            vec![Dependency::StateVarArrayCorrespondingElement {
                array_state_var: ComponentRefArrayRelative(component_ref.into(), array_state_var_name),
            }]
        },

        DependencyInstruction::Parent { state_var } => {

            let parent_name = component.parent.clone().expect(&format!(
                "Component {} doesn't have a parent, but the dependency instruction {}:{} asks for one.",
                    component.name, state_var_slice, instruction_name
            ));

            // Look up what kind of child state var it is
            // If the state var is an array, depend on the array, otherwise as normal
            let parent_component = components.get(&parent_name).unwrap();
            let sv_def = parent_component.definition.state_var_definitions.get(state_var).unwrap();
            let sv_slice = if sv_def.is_array() {
                    StateVarSlice::Array(state_var)
                } else {
                    StateVarSlice::Single(StateRef::Basic(state_var))
                };

            let component_states = ComponentGroupSliceRelative(ComponentGroup::Single(ComponentRef::Basic(parent_name)).into(), sv_slice);
            vec![Dependency::StateVar { component_states }]
        },

        DependencyInstruction::Child { desired_profiles, parse_into_expression } => {

            enum RelevantChild<'a> {
                StateVar(Dependency),
                String(&'a String, &'a ComponentName), // value, parent name
            }

            let mut relevant_children: Vec<RelevantChild> = Vec::new();
            let mut can_parse_into_expression = *parse_into_expression;
            
            let source_relative =
                get_recursive_copy_source_component_when_exists(components, &component_slice.0);
            let source = components.get(&source_relative.0).unwrap();
            
            if let Some(CopySource::StateVar(ref component_state_relative)) = source.copy_source {
                // copying a state var means we don't inheret its children,
                // so we depend on it directly
                let component_states = ComponentGroupSliceRelative(ComponentGroupRelative(ComponentGroup::Single(component_state_relative.0.0.clone()), component_state_relative.0.1.clone()), StateVarSlice::Single(component_state_relative.1.clone()));
                relevant_children.push(
                    RelevantChild::StateVar(Dependency::StateVar { component_states })
                );
            } else if let Some(CopySource::DynamicElement(ref source_comp, source_sv, _, _)) = source.copy_source {
                relevant_children.push(
                    RelevantChild::StateVar(Dependency::StateVarArrayDynamicElement {
                        array_state_var: ComponentRefArrayRelative(ComponentRef::Basic(source_comp.clone()).into(), source_sv),
                        index_state_var: StateRef::Basic(PROP_INDEX_SV),
                    })
                );
            } else if let Some(CopySource::Component(ref component_ref_relative)) = source.copy_source {
                if matches!(component_ref_relative.0, ComponentRef::BatchMember(_, _, _)) {
                    // a batch member has no children, so we depend on it directly
                    let component_states = ComponentGroupSliceRelative(ComponentGroupRelative(ComponentGroup::Single(component_ref_relative.0.clone()), component_ref_relative.1.clone()), state_var_slice.clone());
                    relevant_children.push(
                        RelevantChild::StateVar(Dependency::StateVar { component_states })
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
                            let component_states = ComponentGroupSliceRelative(ComponentGroupRelative(child_group, source_relative.1.clone()), profile_sv_slice);
                            relevant_children.push(
                                RelevantChild::StateVar(Dependency::StateVar { component_states })
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

            let mut dependencies = Vec::new();

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
            
            dependencies
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

            let attribute = component_attributes.get(*attribute_name);
            if attribute.is_none() {
                if let Some(CopySource::Component(component_ref_relative)) = &component.copy_source {

                    // inherit attribute from copy source
                    let component_states = ComponentGroupSliceRelative(ComponentGroupRelative(component_ref_relative.0.clone().into(),component_ref_relative.1.clone()),StateVarSlice::Single(state_var_ref));
                    return vec![Dependency::StateVar { component_states }]
                }

                if should_initialize_essential_data {
                    create_essential_data_for(
                        &component.name,
                        EssentialDataOrigin::StateVar(state_var_name),
                        InitialEssentialData::Single(default_value),
                        essential_data
                    );    
                }

                return vec![Dependency::Essential {
                    component_name: component.name.clone(),
                    origin: essential_origin,
                }]
            }

            // attribute specified
            let attribute = attribute.unwrap();

            log_debug!("attribute {:?}", attribute);

            // Create the essential data if it does not exist yet
            if should_initialize_essential_data && !essential_data_exists_for(&component.name, &essential_origin, essential_data) {

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

                let initial_essential_data;
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

                    initial_essential_data = InitialEssentialData::Array(essential_attr_objs, default_value);

                } else {

                    assert_eq!(attribute.keys().len(), 1);
                    let obj_list = attribute.get(&1).unwrap();

                    log_debug!("Initializing non-array essential data for {}:{} from attribute data {:?}", component.name, state_var_name, obj_list);

                    let value = get_value_from_object_list(obj_list);
                    initial_essential_data = InitialEssentialData::Single(value);                    
                }

                create_essential_data_for(
                    &component.name,
                    essential_origin.clone(),
                    initial_essential_data,
                    essential_data,
                );
            }



            if matches!(index, StateIndex::SizeOf) {
                // size does not depend on referenced objects
                return vec![Dependency::Essential {
                    component_name: component.name.clone(),
                    origin: essential_origin,
                }]
            }

            let attribute_index = match index {
                StateIndex::Element(i) => *i,
                _ => 1,
            };

            let attr_objects = attribute.get(&attribute_index)
                .expect(&format!("attribute {}:{} does not have index {}. Attribute: {:?}",
                    &component.name, attribute_name, &attribute_index, attribute));

            let mut dependencies = Vec::new();

            let relevant_attr_objects = match sv_def {
                StateVarVariant::Number(_) |
                StateVarVariant::NumberArray(_) |
                StateVarVariant::Integer(_) => {
                    // First add an essential dependency to the expression
                    dependencies.push(Dependency::Essential {
                        component_name: component.name.clone(),
                        origin: essential_origin.clone(),
                    });

                    attr_objects.into_iter().filter_map(|obj|
                        matches!(obj, ObjectName::Component(_)).then(|| obj.clone())
                    ).collect()
                },
                _ => attr_objects.clone(),
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

                        let component_states = ComponentGroupSliceRelative(ComponentGroup::Single(ComponentRef::Basic(comp_name.clone())).into(),StateVarSlice::Single(StateRef::Basic(primary_input_sv)));
                        Dependency::StateVar { component_states }
                    },
                };

                dependencies.push(dependency);
            }

            dependencies
        },
    }
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
    let component_slice = ComponentStateSliceAllInstances(component.name.clone(), StateVarSlice::Single(StateRef::Basic(PROP_INDEX_SV)));
    dependencies.insert(
        DependencyKey(component_slice.clone(), PROP_INDEX_VARS_INSTRUCTION),
        variable_components.iter().map(|comp_name| {
            let component_states = ComponentGroupSliceRelative(ComponentGroup::Single(ComponentRef::Basic(comp_name.to_string())).into(),StateVarSlice::Single(StateRef::Basic("value")));
            Dependency::StateVar { component_states }
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
        DependencyKey(component_slice, PROP_INDEX_EXPR_INSTRUCTION),
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




/// An instance is specified with a component to refer to a single map instance.
/// The length is the number of maps a component is inside.
/// Each index selects the map instance the component is part of.
/// Note: instance number starts at 1
pub type Instance = Vec<usize>;

/// When referring to multiple instances, the vector is shorter than the number of maps
/// instancing the component. This is used to refer to every instance across the omitted maps.
pub type InstanceGroup = Vec<usize>;

/// Used to find instances relative to another component
/// Useful for copy sources, for example:
/// If A, a component inside a map,
/// copies B, a component inside a map in the map,
/// While each instance of A copies a different instance of B,
/// their relative instance is the same.
pub type RelativeInstance = Vec<usize>;

// Components
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ComponentNodeInstance (ComponentName, Instance);

#[derive(Debug, Clone)]
struct ComponentNodeRelative (ComponentName, RelativeInstance);

#[derive(Debug, Clone)]
struct ComponentRefInstance (ComponentRef, Instance);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComponentRefRelative (ComponentRef, RelativeInstance);

#[derive(Debug, Clone)]
struct ComponentGroupInstance (ComponentGroup, Instance);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ComponentGroupRelative (ComponentGroup, RelativeInstance);

#[derive(Debug)]
struct RenderedComponent {
    component_ref_instance: ComponentRefInstance,
    child_of_copy: Option<ComponentName>,
}

// Single state variables
#[derive(Debug, Clone)]
struct ComponentState(ComponentNodeInstance, StateRef);

#[derive(Debug, Clone)]
struct ComponentStateSlice (ComponentNodeInstance, StateVarSlice);

#[derive(Debug, Clone)]
struct ComponentRefSlice (ComponentRefInstance, StateVarSlice);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComponentRefArrayRelative (ComponentRefRelative, StateVarName);

#[derive(Debug, Clone)]
struct EssentialState (ComponentNodeInstance, EssentialDataOrigin, StateIndex);

// State variable slices
#[derive(Debug, Clone)]
pub struct ComponentRefStateRelative (ComponentRefRelative, StateRef);

#[derive(Debug, Clone)]
struct ComponentRefSliceRelative (ComponentRefRelative, StateVarSlice);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComponentGroupSliceRelative (ComponentGroupRelative, StateVarSlice);


// Instance independent
#[derive(Debug, Clone)]
struct ComponentStateAllInstances (ComponentName, StateRef);

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize)]
struct ComponentStateSliceAllInstances (ComponentName, StateVarSlice);


// Multiple states and instances
#[derive(Debug, Clone)]
struct ComponentInstancesSlice (ComponentName, InstanceGroup, StateVarSlice);




// ==== Significant Conversions (they require &DoenetCore) ====

impl ComponentGroupInstance {

    /// Converts component group to a vector of component references.
    fn component_group_members(self, core: &DoenetCore) -> Vec<ComponentRefInstance> {
        let instance = self.1.clone();
        match self.0 {
            ComponentGroup::Single(comp_ref) => vec![ComponentRefInstance(comp_ref, instance)],
            ComponentGroup::Batch(name) =>
                indices_for_size(resolve_batch_size(core, &ComponentNodeInstance(name.clone(), instance.clone()), None))
                    .map(|i| ComponentRefInstance(ComponentRef::BatchMember(name.clone(), None, i), instance.clone())).collect(),
            ComponentGroup::Collection(name) =>
                indices_for_size(collection_size(core, &ComponentNodeInstance(name.clone(), instance.clone())))
                    .map(|i| ComponentRefInstance(ComponentRef::CollectionMember(name.clone(), i), instance.clone())).collect(),
        }
    }
}

impl ComponentRefSlice {
    /// Convert (ComponentRef, Instance, StateVarSlice) -> (ComponentName, Instance, StateVarSlice).
    /// If the component reference is a group member, these are not the same.
    fn convert_component_ref_state_var(self, core: &DoenetCore) -> Option<ComponentStateSlice> {
        let component_ref_instance = self.0;
        let (name, map) = match component_ref_instance.component_ref_apply_collection(core) {
            Some(ComponentRefInstance(c,m)) => (c, m),
            None => return None,
        };

        match name {
            ComponentRef::Basic(n) => Some(ComponentStateSlice(ComponentNodeInstance(n, map), self.1)),
            ComponentRef::BatchMember(n, c, i) => batch_state_var(core, &ComponentStateSliceAllInstances(n.clone(), self.1), c,i)
                .map(|sv| ComponentStateSlice(ComponentNodeInstance(n, map), sv)),
            _ => None,
        }
    }
}

impl ComponentRefInstance {
    // Returns the component that the ComponentRef refers to.
    // If the ComponentRef refers to a batch member, this returns the entire component.
    fn component_ref_original_component(self, core: &DoenetCore) -> ComponentNodeInstance {
        let ComponentRefInstance(c,m) = self.clone().component_ref_apply_collection(core).expect(
            &format!("no component original {:?}", self)
        );
        ComponentNodeInstance(c.name(), m)
    }

    // Converts a collection into the ComponentRef it points to.
    // Returns ComponentRef of type basic or batch.
    fn component_ref_apply_collection(self, core: &DoenetCore) -> Option<Self> {
        match self.0 {
            ComponentRef::CollectionMember(n, i) => nth_collection_member(core, &ComponentNodeInstance(n, self.1), i),
            _ => Some(self),
        }
    }
}

impl ComponentRefRelative {
    fn instance_relative_to(
        self,
        component_nodes: &HashMap<ComponentName, ComponentNode>,
        component_node_instance: &ComponentNodeInstance,
    ) -> ComponentRefInstance {
        let component_node_relative = ComponentNodeRelative(self.0.name(), self.1.clone());
        let (group, sources) = instance_relative_to_group_internal(component_nodes, component_node_instance, &component_node_relative);
        assert_eq!(group.len(), sources.len());
        ComponentRefInstance(self.0.clone(), group)
    }
}

impl ComponentNodeRelative {
    fn instance_relative_to(
        self,
        component_nodes: &HashMap<ComponentName, ComponentNode>,
        component_node_instance: &ComponentNodeInstance,
    ) -> ComponentNodeInstance {
        let (group, sources) = instance_relative_to_group_internal(component_nodes, component_node_instance, &self);
        assert_eq!(group.len(), sources.len());
        ComponentNodeInstance(self.0, group)
    }
}

impl ComponentGroupRelative {
    fn instance_relative_to(self, core: &DoenetCore, component_node_instance: &ComponentNodeInstance)
        -> Vec<ComponentGroupInstance> {

        let component_node_relative = ComponentNodeRelative(self.0.name(), self.1.clone()); 
        let (group, sources) = instance_relative_to_group_internal(&core.component_nodes, component_node_instance, &component_node_relative);
        let instances = all_map_instances(core, &group, &sources[group.len()..]);
        instances.into_iter().map(|i| ComponentGroupInstance(self.0.clone(), i)).collect()
    }
}



// ==== Other implementations ====

impl ComponentInstancesSlice {
    fn collapse_instance(&self, instance: Instance) -> ComponentStateSlice {
        ComponentStateSlice(ComponentNodeInstance(self.0.clone(), instance), self.2.clone())
    }
}

impl TryFrom<ComponentStateSlice> for ComponentState {
    type Error = &'static str;
    fn try_from(value: ComponentStateSlice) -> Result<Self, Self::Error> {
        match value.1 {
            StateVarSlice::Single(state_ref) => Ok(ComponentState(value.0, state_ref)),
            StateVarSlice::Array(_) => Err("array")
        }
    }
}

impl ComponentState {
    fn ignore_instance(self) -> ComponentStateAllInstances {
        ComponentStateAllInstances(self.0.0, self.1)
    }
    fn new_index(self, index: StateIndex) -> ComponentState {
        ComponentState(self.0, StateRef::from_name_and_index(self.1.name(), index))
    }
    fn replace_state_var(self, state_ref: StateRef) -> Self {
        ComponentState(self.0, state_ref)
    }
}

impl ComponentStateSlice {
    fn index(self, index: StateIndex) -> ComponentState {
        match (&self.1, index) {
            (StateVarSlice::Single(n), StateIndex::Basic) => ComponentState(self.0, n.clone()),
            (_, StateIndex::Basic) |
            (StateVarSlice::Single(_), _) => panic!(),
            (StateVarSlice::Array(n), i) => ComponentState(self.0, StateRef::from_name_and_index(n,i)),
        }
    }
}

impl ComponentRefArrayRelative {
    fn split_array_with_index(self, index: StateIndex) -> (ComponentRefRelative, StateVarSlice) {
        (self.0, StateVarSlice::Single(StateRef::from_name_and_index(self.1, index)))
    }
}

impl ComponentGroupSliceRelative {
    fn split_slice(self) -> (ComponentGroupRelative, StateVarSlice) {
        (self.0, self.1)
    }
}

impl From<ComponentRef> for ComponentRefRelative {
    fn from(value: ComponentRef) -> Self {
        ComponentRefRelative(value, RelativeInstance::default())
    }
}

impl From<ComponentGroup> for ComponentGroupRelative {
    fn from(value: ComponentGroup) -> Self {
        ComponentGroupRelative(value, RelativeInstance::default())
    }
}

impl ComponentNodeInstance {
    /// Used for action names
    fn alias(&self) -> String {
        if self.1.len() > 0 {
            format!("{:?}{}", self.1, self.0)
        } else {
            self.0.to_string()
        }
    }

    fn dealias(alias: &String) -> ComponentNodeInstance {
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
            ComponentNodeInstance(name, instance)
        } else {
            ComponentNodeInstance(alias.clone(), Instance::default())
        }
    }
}





fn resolve_state_variable(
    core: &DoenetCore,
    state_variable: &ComponentState,
) -> Option<StateVarValue> {

    if let StateRef::ArrayElement(..) = &state_variable.1 {
        let size_variable = state_variable.clone().new_index(StateIndex::SizeOf);
        resolve_state_variable(core, &size_variable);
    }

    let state_vars = core.component_states.get(&state_variable.0.0).unwrap();

    // No need to continue if the state var is already resolved or if the element does not exist
    let current_state = state_vars.get(&state_variable.1.name())
        .expect(&format!("Component {} has no state var '{:?}'", state_variable.0.0, state_variable.1))
        .get_single_state(&state_variable.1.index(), &state_variable.0.1)
        .expect(&format!("Error accessing state of {:?}", state_variable));
    if let Some(State::Resolved(current_value)) = current_state {
        return Some(current_value);
    } else if current_state.is_none() {
        // There is nothing to resolve
        log_debug!("{:?} does not exist", state_variable);
        return None
    }

    log_debug!("Resolving {:?}", state_variable);

    let my_dependencies = dependencies_of_state_var(&core.dependencies, &state_variable.clone().ignore_instance());
    log_debug!("Dependencies of ", state_variable);

    let mut dependency_values: HashMap<InstructionName, Vec<DependencyValue>> = HashMap::new();
    for (dep_name, deps) in my_dependencies {
        let mut values_for_this_dep: Vec<DependencyValue> = Vec::new();

        for dep in deps {
            let dependency_source = get_source_for_dependency(core, &dep, &core.essential_data);

            match dep {
                Dependency::StateVar { component_states } => {

                    let (component_group_relative, component_group_sv_slice) = component_states.split_slice();
                    let group_instances = component_group_relative.instance_relative_to(core, &state_variable.0);

                    for group_instance in group_instances {
                        for component_ref_instance in group_instance.component_group_members(core) {

                            let comp_ref_slice = ComponentRefSlice(component_ref_instance, component_group_sv_slice.clone());
                            let sv_slice = comp_ref_slice.convert_component_ref_state_var(core).unwrap();

                            values_for_this_dep.extend(
                                get_dependency_values_for_state_var_slice(core, &sv_slice)
                            );
                        }
                    }

                },

                Dependency::UndeterminedChildren { component_name , desired_profiles } => {

                    let group_members = ComponentGroupInstance(ComponentGroup::Collection(component_name.clone()), state_variable.0.1.clone()).component_group_members(core);

                    let mut children = Vec::new();
                    for component_ref_instance in group_members {
                        children.extend(state_vars_for_undetermined_children(core, &component_ref_instance, &desired_profiles));
                    }
                    for (object_slice, dep_source,) in children {
                        match object_slice {
                            ObjectStateVarInstance::Component(slice_variable) => {
                                values_for_this_dep.extend(
                                    get_dependency_values_for_state_var_slice(core, &slice_variable)
                                );

                            },
                            ObjectStateVarInstance::String(s) => {
                                values_for_this_dep.push(DependencyValue {
                                    source: dep_source,
                                    value: StateVarValue::String(s),
                                })
                            },
                        };
                    }

                },

                Dependency::MapSources { map_sources, state_var_slice } => {

                    let member = map_sources_dependency_member(core, &state_variable.0, &map_sources);

                    if let Some(component_ref_instance) = member {
                        log_debug!("map source ref: {:?} map{:?}", component_ref, comp_map);

                        let comp_ref_slice = ComponentRefSlice(component_ref_instance, state_var_slice);
                        let sv_slice = comp_ref_slice.convert_component_ref_state_var(core).unwrap();

                        values_for_this_dep.extend(
                            get_dependency_values_for_state_var_slice(core, &sv_slice)
                        );
                    }
                },
                Dependency::StateVarArrayCorrespondingElement { array_state_var } => {

                    let (component_ref_relative, sv_slice) = array_state_var.split_array_with_index(state_variable.1.index());
                    let component_ref_instance = component_ref_relative.instance_relative_to(&core.component_nodes, &state_variable.0);

                    let comp_ref_slice = ComponentRefSlice(component_ref_instance, sv_slice.clone());
                    let sv_slice = comp_ref_slice.convert_component_ref_state_var(core).unwrap();

                    values_for_this_dep.extend(
                        get_dependency_values_for_state_var_slice(core, &sv_slice)
                    );
                },

                Dependency::Essential { component_name, origin } => {

                    let component_node_relative = ComponentNodeRelative(component_name.clone(), RelativeInstance::default());
                    let dependency_map = component_node_relative.instance_relative_to(&core.component_nodes, &state_variable.0);

                    let index = match origin {
                        EssentialDataOrigin::StateVar(_) => state_variable.1.index(),
                        _ => StateIndex::Basic,
                    };

                    let value = core.essential_data
                        .get(&dependency_map.0).unwrap()
                        .get(&origin).unwrap()
                        .clone()
                        .get_value(index, &dependency_map.1);
    
                    if let Some(value) = value {
                        values_for_this_dep.push(DependencyValue {
                            source: dependency_source,
                            value,
                        })
                    }
                },

                Dependency::StateVarArrayDynamicElement { array_state_var, index_state_var } => {

                    let index_variable = state_variable.clone().replace_state_var(index_state_var);
                    let index_value = resolve_state_variable(core, &index_variable);

                    let index: Option<usize> = index_value.and_then(|i|
                        convert_float_to_usize(i.try_into().unwrap())
                    );

                    if let Some(index) = index {

                        log_debug!("got prop index which is {}", index);

                        let (component_ref_relative, sv_slice) = array_state_var.split_array_with_index(StateIndex::Element(index));
                        let component_ref_instance = component_ref_relative.instance_relative_to(&core.component_nodes, &state_variable.0);

                        let slice_variable = ComponentRefSlice(component_ref_instance, sv_slice);
                        let slice_variable = slice_variable.convert_component_ref_state_var(core).unwrap();

                        values_for_this_dep.extend(
                            get_dependency_values_for_state_var_slice(core, &slice_variable)
                        );
                    }
                }
            }
        }

        dependency_values.insert(dep_name, values_for_this_dep);
    }


    log_debug!("Dependency values for {:?}: {:#?}", state_variable, dependency_values);

    let node = core.component_nodes.get(&state_variable.0.0).unwrap();

    let update_instruction = generate_update_instruction_for_state_ref(
        core,
        state_variable,
        dependency_values,
    ).expect(&format!("Can't resolve {:?} (a {} component type)",
        state_variable, node.definition.component_type)
    );

    let new_value = handle_update_instruction(state_variable, state_vars, update_instruction);

    return new_value;
}

fn resolve_slice(
    core: &DoenetCore,
    sv_slice: ComponentStateSlice,
) -> Vec<Option<StateVarValue>> {
    match &sv_slice.1 {
        StateVarSlice::Single(_) => {
            let component_state = sv_slice.index(StateIndex::Basic);
            vec![resolve_state_variable(core, &component_state)]
        }
        StateVarSlice::Array(_) => {
            // resolve the size before the elements
            let size_variable = sv_slice.clone().index(StateIndex::SizeOf);
            let size_value: usize = resolve_state_variable(core, &size_variable)
            .expect("Array size should always resolve to a StateVarValue")
            .try_into().unwrap();
            
            indices_for_size(size_value).map(|id| {
                let element_variable = sv_slice.clone().index(StateIndex::Element(id));
                resolve_state_variable(core, &element_variable)
            }).collect()
        }
    }
}

/// This determines the state var given its dependency values.
fn generate_update_instruction_for_state_ref(
    core: &DoenetCore,
    component_state: &ComponentState,
    dependency_values: HashMap<InstructionName, Vec<DependencyValue>>

) -> Result<StateVarUpdateInstruction<StateVarValue>, String> {

    if component_state.1.name() == PROP_INDEX_SV {
        prop_index_determine_value(dependency_values).map(|update_instruction| match update_instruction {
            StateVarUpdateInstruction::NoChange => StateVarUpdateInstruction::NoChange,
            StateVarUpdateInstruction::SetValue(num_val) => StateVarUpdateInstruction::SetValue(num_val.into()),
        })
    } else {

        let state_var_def = core.component_nodes.get(&component_state.0.0).unwrap().definition
            .state_var_definitions.get(component_state.1.name()).unwrap();

        match component_state.1 {
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
    component_state: &ComponentState,
    component_state_vars: &HashMap<StateVarName, StateForStateVar>,
    instruction: StateVarUpdateInstruction<StateVarValue>
) -> Option<StateVarValue> {

    // log_debug!("handling update instruction {:?}", &instruction);

    let map = &component_state.0.1;
    let state_var_ref = &component_state.1;

    let state_var = component_state_vars.get(state_var_ref.name()).unwrap();

    let updated_value: Option<StateVarValue>;

    match instruction {
        StateVarUpdateInstruction::NoChange => {
            let current_value = component_state_vars.get(state_var_ref.name()).unwrap()
                .get_single_state(&state_var_ref.index(), map)
                .expect(&format!("Error accessing state of {:?}", component_state));

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
    sv_slice: &ComponentStateSlice,
) -> Vec<DependencyValue> {

    let source = DependencySource::StateVar {
        component_type: core.component_nodes.get(&sv_slice.0.0).unwrap().definition.component_type,
        state_var_name: sv_slice.1.name()
    };

    resolve_slice(core, sv_slice.clone())
        .into_iter()
        .filter_map(|v_opt|
            v_opt.map(|value| DependencyValue {
                source: source.clone(),
                value,
            })
        ).collect()
}





// TODO: Use &Dependency instead of cloning
fn dependencies_of_state_var(
    dependencies: &HashMap<DependencyKey, Vec<Dependency>>,
    component_state: &ComponentStateAllInstances,
) -> HashMap<InstructionName, Vec<Dependency>> {
    let component_name = &component_state.0;
    let state_ref = &component_state.1;

    let deps = dependencies.iter().filter_map(| (key, deps) | {

        let key_is_me = &key.0.0 == component_name && (
            key.0.1 == StateVarSlice::Single(state_ref.clone())
            || matches!(state_ref, StateRef::ArrayElement(_, _))
            && key.0.1 == StateVarSlice::Array(state_ref.name())
        );

        key_is_me.then(|| (key.1, deps))
    });

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

        Dependency::StateVarArrayCorrespondingElement { array_state_var } => {
            let component_type = component_ref_definition(
                &core.component_nodes,
                &array_state_var.0.0,
            ).component_type;

            DependencySource::StateVar {
                component_type,
                state_var_name: array_state_var.1,
            }
        }
        Dependency::StateVar { component_states } => {
            let component_type = group_member_definition(
                &core.component_nodes,
                &component_states.0.0,
            ).component_type;

            DependencySource::StateVar {
                component_type,
                state_var_name: component_states.1.name()
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

        Dependency::StateVarArrayDynamicElement { array_state_var, .. } => {
            let component_type =
                component_ref_definition(&core.component_nodes, &array_state_var.0.0)
                .component_type;
            DependencySource::StateVar {
                component_type,
                state_var_name: &array_state_var.1
            }
        }

    }
}

/// Also includes the values of essential data
fn get_dependency_sources_for_state_var(
    core: &DoenetCore,
    component_state: &ComponentState,
) -> HashMap<InstructionName, Vec<(DependencySource, Option<StateVarValue>)>> {

    let component_name = &component_state.0.0;
    let map = &component_state.0.1;
    let state_ref = &component_state.1;
    
    let my_dependencies = dependencies_of_state_var(&core.dependencies, &component_state.clone().ignore_instance());
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

#[derive(Debug, Clone)]
enum ObjectStateVarInstance {
    String(String),
    Component(ComponentStateSlice),
}

fn state_vars_for_undetermined_children(
    core: &DoenetCore,
    component_ref_instance: &ComponentRefInstance,
    desired_profiles: &Vec<ComponentProfile>,
) -> Vec<(ObjectStateVarInstance, DependencySource)> {
    let mut source_and_value = vec![];

    for (member_child, comp_map, _) in get_children_and_members(core, component_ref_instance) {
        match member_child {
            ObjectRefName::Component(child_ref) => {

                let child_def = component_ref_definition(&core.component_nodes, &child_ref);
                let child_ref_instance = ComponentRefInstance(child_ref.clone(), comp_map.clone());

                match  &child_def.replacement_components {
                    Some(ReplacementComponents::Children) => {
                        source_and_value.extend(state_vars_for_undetermined_children(core, &child_ref_instance, desired_profiles));
                        continue;
                    }
                    _ => (),
                };
                        
                if let Some(relevant_sv) = child_def.component_profile_match(&desired_profiles) {
                    let comp_ref_slice = ComponentRefSlice(ComponentRefInstance(child_ref, comp_map.clone()), relevant_sv);
                    let sv_slice = comp_ref_slice.convert_component_ref_state_var(core).unwrap();

                    let dependency_source = DependencySource::StateVar {
                        component_type: child_def.component_type,
                        state_var_name: sv_slice.1.name()
                    };
                    source_and_value.push((ObjectStateVarInstance::Component(sv_slice), dependency_source));
                }
            },
            ObjectRefName::String(s) => {
                let dependency_source = DependencySource::StateVar {
                    component_type: "string",
                    state_var_name: "",
                };
                source_and_value.push((ObjectStateVarInstance::String(s), dependency_source));
            },
        };
    }
    source_and_value
}






fn mark_stale_state_var_and_dependencies(
    core: &DoenetCore,
    component_states: &ComponentInstancesSlice,
) {
    let state = core.component_states.get(&component_states.0).unwrap()
        .get(&component_states.2.name()).unwrap();
    let instances = state.instances_where_slice_is_resolved(&component_states.2, &component_states.1);

    log_debug!("Check stale {:?}", component_states);
    for instance in instances {

        let instance = instance
            .expect(&format!("Error accessing state of {:?}", component_states));
        log_debug!("Marking stale {:?}", component_states);

        state.mark_stale_slice(&component_states.2, &instance);

        let component_state_slice = component_states.collapse_instance(instance);
        let depending_on_me = get_state_variables_depending_on_me(core, &component_state_slice);

        for component_instances_slice in depending_on_me {
            mark_stale_state_var_and_dependencies(core, &component_instances_slice);
        }
    }
}

fn mark_stale_essential_datum_dependencies(
    core: &DoenetCore,
    essential_state: &EssentialState,
) {
    let component_name = essential_state.0.0.clone();
    let map = &essential_state.0.1;
    let origin = essential_state.1.clone();
    let state_index = &essential_state.2;

    // log_debug!("Marking stale essential {}:{}", component_name, state_var);

    let search_dep = Dependency::Essential {
        component_name,
        origin,
    };

    let my_dependencies = core.dependencies.iter().filter_map( |(key, deps) | {
        if deps.contains(&search_dep) {
            let state_ref_option = match &key.0.1 {
                StateVarSlice::Single(s) => Some(s.clone()),
                StateVarSlice::Array(_) => key.0.1.clone().specify_index(state_index.clone())
            };
            state_ref_option.map (|state_var_ref|
                ComponentInstancesSlice(key.0.0.clone(), map.clone(), StateVarSlice::Single(state_var_ref))
            )
        } else {
            None
        }
    });

    for component_instances_slice  in my_dependencies {
        mark_stale_state_var_and_dependencies(core, &component_instances_slice);
    }
}

/// Calculate all the state vars that depend on the given state var
fn get_state_variables_depending_on_me(
    core: &DoenetCore,
    component_states: &ComponentStateSlice,
) -> Vec<ComponentInstancesSlice> {

    let sv_component = &component_states.0.0;
    let sv_slice = &component_states.1;

    let mut depending_on_me = vec![];

    for (dependency_key, dependencies) in core.dependencies.iter() {
        for dependency in dependencies {

            match dependency {
                Dependency::StateVar { component_states: component_group_states } => {
                    let slice_depends = match &component_group_states.0.0 {
                        ComponentGroup::Single(ComponentRef::BatchMember(n, b, i)) => {
                            let state_var_slice = batch_state_var(
                                core,
                                &ComponentStateSliceAllInstances(n.clone(),
                                component_group_states.1.clone()),
                                *b,
                                *i
                            ).unwrap();
                            slice_depends_on_slice(&state_var_slice, sv_slice)
                        },
                        ComponentGroup::Batch(n) => {
                            // TODO: check if any index depends on sv_slice, not just some range
                            (1..4)
                                .filter_map(|i| batch_state_var(
                                    core,
                                    &ComponentStateSliceAllInstances(n.clone(),
                                    component_group_states.1.clone()),
                                    None,
                                    i)
                                ).any(|s| slice_depends_on_slice(&s, sv_slice))
                        },
                        _ => slice_depends_on_slice(&component_group_states.1, sv_slice),
                    };

                    if group_includes_component(&core.group_dependencies, &component_group_states.0.0, sv_component)
                    && slice_depends {

                        let instance_group = instance_relative_to_group(core, &component_states.0, &ComponentNodeRelative(dependency_key.0.0.clone(), component_group_states.0.1.clone()));
                        depending_on_me.push(
                            ComponentInstancesSlice(dependency_key.0.0.clone(), instance_group, dependency_key.0.1.clone())
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
                        let instance_group = instance_relative_to_group(
                            core, &component_states.0, &ComponentNodeRelative(dependency_key.0.0.clone(), RelativeInstance::default()));
                        depending_on_me.push(
                            ComponentInstancesSlice(dependency_key.0.0.clone(), instance_group, dependency_key.0.1.clone())
                        );
                    }
                },

                Dependency::MapSources { map_sources, state_var_slice } => {
                    if sv_component == map_sources
                    && slice_depends_on_slice(state_var_slice, sv_slice) {

                        let instance_group = instance_relative_to_group(
                            core, &component_states.0, &ComponentNodeRelative(dependency_key.0.0.clone(), RelativeInstance::default()));
                        depending_on_me.push(
                            ComponentInstancesSlice(dependency_key.0.0.clone(), instance_group, dependency_key.0.1.clone())
                        );
                    }
                },

                Dependency::StateVarArrayCorrespondingElement { array_state_var } => {
                    if group_includes_component(&core.group_dependencies, &ComponentGroup::Single(array_state_var.0.0.clone()), sv_component)
                    && array_state_var.1 == sv_slice.name() {

                        let dependent_slice = sv_slice;
                        let instance_group = instance_relative_to_group(
                            core, &component_states.0, &ComponentNodeRelative(dependency_key.0.0.clone(), RelativeInstance::default()));
                        depending_on_me.push(
                            ComponentInstancesSlice(dependency_key.0.0.clone(), instance_group, dependent_slice.clone())
                        );
                    }
                },

                Dependency::StateVarArrayDynamicElement { array_state_var, .. } => {

                    let this_array_refers_to_me = 
                        group_includes_component(&core.group_dependencies, &ComponentGroup::Single(array_state_var.0.0.clone()), sv_component)
                        && array_state_var.1 == sv_slice.name();

                    let i_am_prop_index_of_this_dependency = 
                        // The key that this dependency is under is myself
                        // Aka, the index is supposed to be in my component, not another component
                        dependency_key.component_name() == sv_component
                        // I am actually a propIndex, and not some other state var
                        && sv_slice == &StateVarSlice::Single(StateRef::Basic("propIndex"));

                    if this_array_refers_to_me || i_am_prop_index_of_this_dependency {

                        let instance_group = instance_relative_to_group(
                            core, &component_states.0, &ComponentNodeRelative(dependency_key.0.0.clone(), RelativeInstance::default()));
                        depending_on_me.push(
                            ComponentInstancesSlice(dependency_key.0.0.clone(), instance_group, dependency_key.0.1.clone())
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
        component_ref_instance: ComponentRefInstance(ComponentRef::Basic(root_node.name.clone()), Instance::default()),
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

    let component_name = component.component_ref_instance.0.name().clone();

    log_debug!("generating render tree for {:?}", component);

    let component_definition = component_ref_definition(
        &core.component_nodes,
        &component.component_ref_instance.0,
    );

    let renderered_state_vars = component_definition
        .state_var_definitions
        .into_iter()
        .filter_map(|(k, v)| {
            v.for_renderer().then(|| match v.is_array() {
                true => StateVarSlice::Array(k),
                false => StateVarSlice::Single(StateRef::Basic(k)),
            })
        });

    let state_var_aliases = match &component_definition.renderer_type {
        RendererType::Special { state_var_aliases, .. } => state_var_aliases.clone(),
        RendererType::Myself => HashMap::new(),
    };

    let mut state_values = serde_json::Map::new();
    for state_var_slice in renderered_state_vars {
        let comp_ref_slice = ComponentRefSlice(component.component_ref_instance.clone(), state_var_slice);
        let sv_slice = comp_ref_slice.convert_component_ref_state_var(core).unwrap();

        let sv_renderer_name = state_var_aliases
            .get(&sv_slice.1.name())
            .map(|x| *x)
            .unwrap_or(sv_slice.1.name())
            .to_string();

        let values = resolve_slice(core, sv_slice.clone());

        let mut json_value = match sv_slice.1 {
            StateVarSlice::Array(_) => json!(values),
            StateVarSlice::Single(_) => json!(values.first().unwrap()),
        };

        // hardcoded exceptions
        if sv_renderer_name == "numbericalPoints"
        && matches!(sv_slice.1, StateVarSlice::Array(_)) {
            let array_2d =
                [[values.get(0).unwrap(), values.get(1).unwrap()],
                [values.get(2).unwrap(), values.get(3).unwrap()]];
            json_value = json!(array_2d)
        }

        state_values.insert(sv_renderer_name, json_value);
    }

    let name_to_render = name_rendered_component(&component, component_definition.component_type);

    let mut children_instructions = Vec::new();
    let node = core.component_nodes.get(&component_name).unwrap();
    if component_definition.should_render_children {
        for (child, child_map, actual_parent) in get_children_and_members(core, &component.component_ref_instance) {
            match child {
                ObjectRefName::String(string) => {
                    children_instructions.push(json!(string));
                },
                ObjectRefName::Component(comp_ref) => {
                    let child_component = RenderedComponent {
                        component_ref_instance: ComponentRefInstance(comp_ref, child_map.clone()),
                        child_of_copy: component.child_of_copy.clone().or(
                            (!std::ptr::eq(actual_parent, node)).then(|| component_name.clone())
                        ),
                    };

                    let child_definition =
                        component_ref_definition(&core.component_nodes, &child_component.component_ref_instance.0);

                    let child_name = name_rendered_component(&child_component, child_definition.component_type);

                    let component_original =
                        child_component.component_ref_instance.clone().component_ref_original_component(core);

                    let action_component_name = component_original.alias();

                    let child_actions: Map<String, Value> =
                        (child_definition.action_names)()
                        .iter()
                        .map(|action_name| (action_name.to_string(), json!({
                            "actionName": action_name,
                            "componentName": action_component_name,
                        }))).collect();

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
    let name_to_render = match &component.component_ref_instance.0 {
        ComponentRef::CollectionMember(n, i) |
        ComponentRef::BatchMember(n, _, i) =>
            format!("__{}_from_({}[{}])", component_type, n, *i),
        _ => component.component_ref_instance.0.name().clone(),
    };
    let name_to_render = match &component.child_of_copy {
        Some(copy_name) => format!("__cp:{}({})", name_to_render, copy_name),
        None => name_to_render,
    };
    let name_to_render = if component.component_ref_instance.1.len() == 0 {
            name_to_render
        } else {
            format!("__{}_map{:?}", name_to_render, component.component_ref_instance.1)
        };
    name_to_render
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
    SetEssentialValue(EssentialState, StateVarValue),
    SetStateVar(ComponentState, StateVarValue),
}

pub fn handle_action_from_json(core: &DoenetCore, action: &str) -> String {

    let (action, action_id) = parse_json::parse_action_from_json(action)
        .expect(&format!("Error parsing json action: {}", action));

    handle_action(core, action);

    action_id
}

pub fn handle_action(core: &DoenetCore, action: Action) {

    log_debug!("Handling action {:#?}", action);

    let component_node_instance  = ComponentNodeInstance::dealias(&action.component_name);

    let component = core.component_nodes.get(&component_node_instance.0)
        .expect(&format!("{} doesn't exist, but action {} uses it", action.component_name, action.action_name));

    let state_var_resolver = | state_var_ref: &StateRef | {
        let component_state = ComponentState(component_node_instance.clone(), state_var_ref.clone());
        resolve_state_variable(core, &component_state)
    };

    let state_vars_to_update = (component.definition.on_action)(
        &action.action_name,
        action.args,
        &state_var_resolver,
    );

    for (state_var_ref, requested_value) in state_vars_to_update {

        let component_state = ComponentState(component_node_instance.clone(), state_var_ref.clone());
        let request = UpdateRequest::SetStateVar(component_state, requested_value);
        process_update_request(core, &request);
    }

    // log_json!("Component tree after action", utils::json_components(&core.component_nodes, &core.component_states));
}


/// Convert the results of `request_dependencies_to_update_value`
/// into UpdateRequest struct.
fn convert_dependency_values_to_update_request(
    core: &DoenetCore,
    component_state: &ComponentState,
    requests: HashMap<InstructionName, Result<Vec<DependencyValue>, String>>,
) -> Vec<UpdateRequest> {

    let component = core.component_nodes.get(&component_state.0.0).unwrap();
    let map = &component_state.0.1;
    let state_var = &component_state.1;

    let my_dependencies = dependencies_of_state_var(&core.dependencies, &component_state.clone().ignore_instance());

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
                    let component_node_instance = ComponentNodeInstance(component_name.clone(), map.clone());
                    update_requests.push(UpdateRequest::SetEssentialValue(
                        EssentialState(component_node_instance, origin.clone(), state_var.index()),
                        request.value.clone(),
                    ))
                },
                Dependency::StateVar { component_states } => {
                    // TODO: recieving multiple dependencies because of multiple instances
                    let component_ref = match &component_states.0.0 {
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

                    let comp_ref_slice = ComponentRefSlice(ComponentRefInstance(component_ref, map.clone()), component_states.1.clone());
                    if let Some(sv_slice) = comp_ref_slice.convert_component_ref_state_var(core) {
                        if let StateVarSlice::Single(state_var_ref) = sv_slice.1 {
                            let component_state = ComponentState(sv_slice.0, state_var_ref);
                            update_requests.push(UpdateRequest::SetStateVar(component_state, request.value.clone()))
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
        UpdateRequest::SetEssentialValue(essential_state, requested_value) => {

            let essential_var = core.essential_data
                .get(&essential_state.0.0).unwrap()
                .get(&essential_state.1).unwrap();

            essential_var.set_value(
                    essential_state.2.clone(),
                    requested_value.clone(),
                    &essential_state.0.1,
                ).expect(
                    &format!("Failed to set essential value for {:?}", essential_state)
                );

            // log_debug!("Updated essential data {:?}", core.essential_data);

            mark_stale_essential_datum_dependencies(core, essential_state);
        },

        UpdateRequest::SetStateVar(component_state, requested_value) => {

            let dep_update_requests = request_dependencies_to_update_value_including_shadow(
                core,
                component_state,
                requested_value.clone(),
            );

            for dep_update_request in dep_update_requests {
                process_update_request(core, &dep_update_request);
            }

            // needed?
            // mark_stale_state_var_and_dependencies(core, component_name, &map, &StateVarSlice::Single(state_var_ref.clone()));
        }
    }
}






fn resolve_batch_size(
    core: &DoenetCore,
    component_node_instance: &ComponentNodeInstance,
    batch_name: Option<BatchName>,
) -> usize {

    // log_debug!("resolving {} batch {:?} found def ", component_name, batch_name);
    let batch_def = core.component_nodes.get(&component_node_instance.0).unwrap()
        .definition.unwrap_batch_def(&batch_name);
    let size_variable = ComponentState(component_node_instance.clone(), batch_def.size.clone());
    resolve_state_variable(core, &size_variable)
        .unwrap().try_into().unwrap()
}

fn collection_size(
    core: &DoenetCore,
    component_node_instance: &ComponentNodeInstance,
) -> usize {

    core.group_dependencies.get(&component_node_instance.0).unwrap()
        .iter()
        .map(|c| collection_members_size(core, c, &component_node_instance.1))
        .sum()
}

fn collection_members_size(
    core: &DoenetCore,
    collection_members: &CollectionMembers,
    map: &Instance,
) -> usize {
    match collection_members {
        CollectionMembers::Component(_) => 1,
        CollectionMembers::Batch(n) => resolve_batch_size(core, &ComponentNodeInstance(n.clone(), map.clone()), None),
        CollectionMembers::ComponentOnCondition { component_name, condition } => {
            let condition_variable = ComponentState(ComponentNodeInstance(component_name.clone(), map.clone()), condition.clone());
            match resolve_state_variable(core, &condition_variable) {
                Some(StateVarValue::Boolean(true)) => 1,
                _ => 0,
            }
        },
        CollectionMembers::InstanceBySources { sources, .. } => collection_size(core, &ComponentNodeInstance(sources.clone(), map.clone())),
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




fn batch_state_var(
    core: &DoenetCore,
    component_slice: &ComponentStateSliceAllInstances,
    batch_name: Option<BatchName>,
    index: usize,
) -> Option<StateVarSlice> {

    let batch_def = core.component_nodes.get(&component_slice.0).unwrap()
        .definition.unwrap_batch_def(&batch_name);
    (batch_def.member_state_var)(index, &component_slice.1)
}

/// The component ref return can either be a basic, or a member of a batch
/// TODO: return is weird
fn nth_collection_member(
    core: &DoenetCore,
    component_node_instance: &ComponentNodeInstance,
    index: usize,
) -> Option<ComponentRefInstance> {

    let mut index = index;
    for c in core.group_dependencies.get(&component_node_instance.0).unwrap() {
        let (size, group_member);
        match c {
            CollectionMembers::Component(component_name) => {
                size = 1;
                group_member = ComponentRefInstance(ComponentRef::Basic(component_name.clone()), component_node_instance.1.clone());
            },
            CollectionMembers::Batch(component_name) => {
                size = resolve_batch_size(core, &ComponentNodeInstance(component_name.clone(), component_node_instance.1.clone()), None);
                group_member = ComponentRefInstance(ComponentRef::BatchMember(component_name.clone(), None, index), component_node_instance.1.clone());
            },
            CollectionMembers::ComponentOnCondition { component_name, condition } => {
                let condition_variable = ComponentState(ComponentNodeInstance(component_name.clone(), component_node_instance.1.clone()), condition.clone());
                let condition = resolve_state_variable(core, &condition_variable);
                size = (condition == Some(StateVarValue::Boolean(true))) as usize;
                group_member = ComponentRefInstance(ComponentRef::Basic(component_name.clone()), component_node_instance.1.clone());
            },
            CollectionMembers::InstanceBySources { sources, template } => {
                size = collection_size(core, &ComponentNodeInstance(sources.clone(), component_node_instance.1.clone()));
                let mut map_new = component_node_instance.1.clone();
                map_new.push(index);
                group_member = ComponentRefInstance(ComponentRef::Basic(template.clone()), map_new)
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




/// Find the component that the sources dependency points to
fn map_sources_dependency_member(
    core: &DoenetCore,
    component_node_instance: &ComponentNodeInstance,
    sources: &ComponentName,
) -> Option<ComponentRefInstance> {
    let sources_for_component = sources_that_instance_component(&core.component_nodes, &component_node_instance.0);
    let sources_index_in_map = sources_for_component.iter().position(|n| n == sources).unwrap();
    let index = *component_node_instance.1.get(sources_index_in_map).unwrap();
    let sources_map = component_node_instance.1[0..sources_index_in_map].to_vec();
    let component_node_instance = ComponentNodeInstance(sources.clone(), sources_map);
    nth_collection_member(core, &component_node_instance, index)
}

// impl ComponentGroupRelative
fn instance_relative_to_group(
    core: &DoenetCore,
    component_node_instance: &ComponentNodeInstance,
    component_node_relative: &ComponentNodeRelative,
) -> InstanceGroup {
    instance_relative_to_group_internal(
        &core.component_nodes,
        component_node_instance,
        component_node_relative,
    ).0
}

fn instance_relative_to_group_internal(
    component_nodes: &HashMap<ComponentName, ComponentNode>,
    component_node_instance: &ComponentNodeInstance,
    component_node_relative: &ComponentNodeRelative,
) -> (InstanceGroup, Vec<ComponentName>) {
    let component = &component_node_instance.0;
    let map = &component_node_instance.1;

    let sources_for_component = sources_that_instance_component(component_nodes, component);
    let sources_for_dependency = sources_that_instance_component(component_nodes, &component_node_relative.0);

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
        combined_map.extend(component_node_relative.1.clone());
        (combined_map, sources_for_dependency)
    }
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
    for i in indices_for_size(collection_size(core, &ComponentNodeInstance(sources_name.clone(), map.clone()))) {
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
    component_ref_instance: &ComponentRefInstance,
) -> impl Iterator<Item=(ObjectRefName, Instance, &'a ComponentNode)> {

    let component_node_instance = component_ref_instance.clone().component_ref_original_component(core);
    let use_map = component_node_instance.1.clone();
    // log_debug!("use component {} {:?}", use_component_name, use_map);

    get_children_including_copy_and_groups(&core, &component_node_instance)
    .into_iter()
    .flat_map(move |(child, actual_parent)| match child {
        ComponentChild::String(s) => vec![(ObjectRefName::String(s.clone()), use_map.clone(), actual_parent)],
        ComponentChild::Component(comp_name) => {

            match &core.component_nodes.get(&comp_name).unwrap().definition.replacement_components {
                Some(ReplacementComponents::Batch(_)) => {
                    let group = ComponentGroup::Batch(comp_name.clone());
                    ComponentGroupInstance(group, use_map.clone()).component_group_members(core).iter().map(|comp_ref|
                        (ObjectRefName::Component(comp_ref.0.clone()),
                        comp_ref.1.clone(),
                        actual_parent)
                    ).collect::<Vec<(ObjectRefName, Instance, &ComponentNode)>>()
                },
                Some(ReplacementComponents::Collection(_)) => {
                    let group = ComponentGroup::Collection(comp_name.clone());
                    ComponentGroupInstance(group, use_map.clone()).component_group_members(core).iter().map(|comp_ref|
                        (ObjectRefName::Component(comp_ref.0.clone()),
                        comp_ref.1.clone(),
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
    component_node_instance: &ComponentNodeInstance,
) -> Vec<(ComponentChild, &'a ComponentNode)> {

    let mut children_vec: Vec<(ComponentChild, &ComponentNode)> = Vec::new();
    let component = core.component_nodes.get(&component_node_instance.0).unwrap();
    match &component.copy_source {
        Some(CopySource::Component(component_ref_relative)) => {
            let component_ref_instance = component_ref_relative.clone().instance_relative_to(&core.component_nodes, component_node_instance);
            let source_instance = component_ref_instance.component_ref_original_component(core);
            children_vec = get_children_including_copy_and_groups(core, &source_instance);
        },
        Some(CopySource::MapSources(map_sources)) => {
            let source_instance = map_sources_dependency_member(core, component_node_instance, &map_sources).unwrap();
            if let ComponentRef::Basic(name) = source_instance.0 {
                let source_instance = ComponentNodeInstance(name, source_instance.1);

                children_vec = get_children_including_copy_and_groups(core, &source_instance);
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
    if let Some(CopySource::Component(ComponentRefRelative(ComponentRef::Basic(ref source), ..))) = component.copy_source {

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
fn get_recursive_copy_source_component_when_exists(
    components: &HashMap<ComponentName, ComponentNode>,
    component_name: &ComponentName,
) -> ComponentNodeRelative {
    let component = components.get(component_name).unwrap();
    match &component.copy_source {
        Some(CopySource::Component(ComponentRefRelative(ComponentRef::Basic(source), instance1))) => {
            let component_relative = get_recursive_copy_source_component_when_exists(
                components,
                source,
            );
            let instance = instance1.clone().into_iter().chain(component_relative.1).collect();
            ComponentNodeRelative(component_relative.0, instance)
        },
        _ => ComponentNodeRelative(component.name.clone(), RelativeInstance::default()),
    }
}


fn request_dependencies_to_update_value_including_shadow(
    core: &DoenetCore,
    component_state: &ComponentState,
    new_value: StateVarValue,
) -> Vec<UpdateRequest> {

    let component = &core.component_nodes.get(&component_state.0.0).unwrap();
    let map = &component_state.0.1;
    let state_var_ref = &component_state.1;

    if let Some(component_ref_slice_relative) = state_var_is_shadowing(core, &component_state.clone().ignore_instance()) {

        // TODO:
        // let instance = apply_relative_instance(core, component_node_instance, dependency, relative_map)
        let instance = map.clone();

        let source_ref_slice = ComponentRefSlice(ComponentRefInstance(component_ref_slice_relative.0.0, instance), component_ref_slice_relative.1);
        let source_state =
            source_ref_slice.convert_component_ref_state_var(core)
            .unwrap();
        let source_state: ComponentState = source_state.try_into().unwrap();
        vec![UpdateRequest::SetStateVar(source_state, new_value)]

    } else {

        let dependency_sources = get_dependency_sources_for_state_var(core, component_state);

        log_debug!("Dependency sources for {}:{}, {:?}", component.name, state_var_ref, dependency_sources);

        let requests = component.definition.state_var_definitions.get(state_var_ref.name()).unwrap()
            .request_dependencies_to_update_value(state_var_ref, new_value, dependency_sources)
            .expect(&format!("Failed requesting dependencies for {}:{}", component.name, state_var_ref));

        log_debug!("{}:{} wants its dependency to update to: {:?}", component.name, state_var_ref, requests);

        let update_requests = convert_dependency_values_to_update_request(core, component_state, requests);

        log_debug!("{}:{} generated update requests: {:#?}", component.name, state_var_ref, update_requests);

        update_requests
    }
}

/// Detect if a state var is shadowing because of a CopySource
/// and has a primary input state variable, which is needed.
fn state_var_is_shadowing(core: &DoenetCore, component_state: &ComponentStateAllInstances)
    -> Option<ComponentRefSliceRelative> {

    let component = core.component_nodes.get(&component_state.0).unwrap();
    let state_var = &component_state.1;
    if let Some(CopySource::StateVar(ref component_relative)) = component.copy_source {
        if let Some(primary_input_state_var) = component.definition.primary_input_state_var {

            if state_var == &StateRef::Basic(primary_input_state_var) {
                Some(ComponentRefSliceRelative(component_relative.0.clone(), StateVarSlice::Single(component_relative.1.clone())))
            } else {
                None
            }
        } else {
            panic!("{} component type doesn't have a primary input state var", component.definition.component_type);
        }

    } else if let Some(CopySource::DynamicElement(ref source_comp, ref source_state_var, ..)) = component.copy_source {
        if let Some(primary_input_state_var) = component.definition.primary_input_state_var {

            if state_var == &StateRef::Basic(primary_input_state_var) {

                Some(ComponentRefSliceRelative(ComponentRefRelative(ComponentRef::Basic(source_comp.to_string()), RelativeInstance::default()), StateVarSlice::Array(source_state_var)))
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
            Some(CopySource::Component(ref component_ref_relative)) => Some((c, &component_ref_relative.0)),
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
    while let Some(CopySource::Component(ref component_ref_relative)) = current_comp.copy_source {

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
            current_comp = components.get(&component_ref_relative.0.name()).unwrap();
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
        let mut chain = vec![(dep_key.0.0.clone(), dep_key.0.1.clone())];
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
        dep_key.0.0 == last_link.0 && dep_key.0.1 == last_link.1
    });

    for (_, dep_list) in my_dependencies {
        for dep in dep_list {
            let new_link = match dep {
                Dependency::StateVar { component_states } => {
                    Some((component_states.0.0.name().clone(), component_states.1.clone()))
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
