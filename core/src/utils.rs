use std::collections::HashMap;

use crate::prelude::*;
use crate::component_prelude::*;
use crate::state_variables::StateVarValue;
use crate::state_var::State;

use crate::ComponentLike;


use lazy_static::lazy_static;





pub fn package_subtree_as_json(
    components: &HashMap<String, ComponentNode>,
    component_states: &HashMap<String, ComponentState>,
    component: &ComponentNode) -> serde_json::Value {

    use serde_json::Value;
    use serde_json::Map;
    use serde_json::json;

    // Children

    let mut children: Map<String, Value> = Map::new();

    for (child_num, child) in component.children.iter().enumerate() {

        let label;
        let child_json;
        match child {
            ComponentChild::Component(comp_child_name) => {
                let comp_child = components.get(comp_child_name).unwrap();
                child_json = package_subtree_as_json(components, component_states, comp_child);
                label = format!("{} {}", child_num, comp_child_name);
            }
            ComponentChild::String(str) => {
                child_json = Value::String(str.to_string());
                label = format!("{}", child_num);
            }
        };


        children.insert(label, child_json);
    }


    // Attributes

    let mut attributes: Map<String, Value> = Map::new();

    let all_attribute_names = component.definition.attribute_definitions().keys();

    for attribute_name in all_attribute_names {

        let attribute_opt = component.attributes.get(&attribute_name);
        
        if let Some(attribute) = attribute_opt {
            let attribute_json = match attribute {
                Attribute::Component(component_name) => {
                    Value::String(component_name.to_string())
                },
                Attribute::Primitive(state_var_value) => {
                    match state_var_value {
                        StateVarValue::String(v) => json!(v),
                        StateVarValue::Number(v) => json!(v),
                        StateVarValue::Integer(v) => json!(v),
                        StateVarValue::Boolean(v) => json!(v),
                    }
                }
            };
    
            attributes.insert(attribute_name.to_string(), attribute_json);
        }
    }



    
    let mut my_json_props: serde_json::Map<String, Value> = serde_json::Map::new();

    my_json_props.insert("children".to_string(), json!(children));
    my_json_props.insert("attributes".to_string(), json!(attributes));
    my_json_props.insert("parent".to_string(),
        match component.parent {
            None => Value::Null,
            Some(ref parent_name) => Value::String(parent_name.into()),
    });
    my_json_props.insert("type".to_string(), Value::String(component.component_type.to_string()));

    my_json_props.insert("copyTarget".to_string(),
        if let Some(ref copy_target_name) = component.copy_target {
            Value::String(copy_target_name.to_string())
        } else {
            Value::Null
        }
    );

    let component_state = component_states.get(&component.name).unwrap();

    match component_state {
        ComponentState::Shadowing(target_name) => {
            my_json_props.insert("shadowing".to_string(), json!(target_name));
        },

        ComponentState::State(state_vars) => {


            for &state_var_name in component.definition.state_var_definitions().keys() {

                let state_var = state_vars.get(state_var_name).unwrap();
        
                my_json_props.insert(
        
                    format!("sv: {}", state_var_name),
        
                    match state_var.get_state() {
                        State::Resolved(value) => match value {
                            StateVarValue::String(v) => json!(v),
                            StateVarValue::Number(v) => json!(v),
                            StateVarValue::Integer(v) => json!(v),
                            StateVarValue::Boolean(v) => json!(v),
                        },
                        State::Stale => Value::Null,
                    }
                );
        
            }


            for (esv_name, essential_state_var) in state_vars.get_essential_state_vars() {

                let essen_value = match essential_state_var.get_value() {
                    Some(value) => match value {
                        StateVarValue::String(v) => json!(v),
                        StateVarValue::Number(v) => json!(v),
                        StateVarValue::Integer(v) => json!(v),
                        StateVarValue::Boolean(v) => json!(v),
                    },
                    None => Value::Null,
                };
                
                // let essen_shadowing = match &essential_state_var.shadowing_component_name {
                //     Some(comp_name) => Value::String(comp_name.to_string()),
                //     None => Value::Null,
                // };
        
                // let essen_shadowing = json!(essential_state_var.shadowing_component_name);
        
                // let essen_shadowed_by = json!(essential_state_var.shadowed_by_component_names);
        
                my_json_props.insert(format!("essen: {}", esv_name),
                    json!(essen_value)
                    // json! ({
                    //     "value": essen_value,
                    //     "shadowing": essen_shadowing,
                    //     "shadowed by": essen_shadowed_by,
                    // })
        
                );
        
            }
        }
    }




    Value::Object(my_json_props)

}