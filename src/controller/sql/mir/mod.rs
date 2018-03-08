use core::{DataType, NodeIndex};
pub use mir::MirNodeRef;
use mir::node::{GroupedNodeType, MirNode, MirNodeType};
use mir::query::MirQuery;
// TODO(malte): remove if possible
pub use mir::FlowNode;
use dataflow::ops::filter::FilterCondition;
use dataflow::ops::join::JoinType;

use nom_sql::{ArithmeticExpression, Column, ColumnSpecification, CompoundSelectOperator,
              ConditionBase, ConditionExpression, ConditionTree, Literal, Operator, SqlQuery,
              TableKey};
use nom_sql::{LimitClause, OrderClause, SelectStatement};
use controller::sql::query_graph::{OutputColumn, QueryGraph};
use controller::sql::query_signature::Signature;

use slog;
use std::collections::{HashMap, HashSet};

use std::ops::Deref;
use std::vec::Vec;

use controller::sql::security::Universe;
use controller::sql::UniverseId;

mod rewrite;
mod security;
mod join;
mod grouped;

fn sanitize_leaf_column(mut c: Column, view_name: &str) -> Column {
    c.table = Some(view_name.to_string());
    c.function = None;
    if c.alias.is_some() && *c.alias.as_ref().unwrap() == c.name {
        c.alias = None;
    }
    c
}

#[derive(Clone, Debug)]
pub struct SqlToMirConverter {
    base_schemas: HashMap<String, Vec<(usize, Vec<ColumnSpecification>)>>,
    current: HashMap<String, usize>,
    log: slog::Logger,
    nodes: HashMap<(String, usize), MirNodeRef>,
    schema_version: usize,

    /// Universe in which the conversion is happening
    universe: Universe,
}

impl Default for SqlToMirConverter {
    fn default() -> Self {
        SqlToMirConverter {
            base_schemas: HashMap::default(),
            current: HashMap::default(),
            log: slog::Logger::root(slog::Discard, o!()),
            nodes: HashMap::default(),
            schema_version: 0,
            universe: Universe::default(),
        }
    }
}

impl SqlToMirConverter {
    pub fn with_logger(log: slog::Logger) -> Self {
        SqlToMirConverter {
            log: log,
            ..Default::default()
        }
    }

    /// Set universe in which the conversion will happen.
    /// We need this, because different universes will have different
    /// security policies and therefore different nodes that are not
    /// represent in the the query graph
    pub fn set_universe(&mut self, universe: Universe) {
        self.universe = universe;
    }

    /// Set the universe to a policy-free universe
    pub fn clear_universe(&mut self) {
        self.universe = Universe::default();
    }

    fn get_view(&self, view_name: &str) -> MirNodeRef {
        let latest_existing = self.current.get(view_name);
        match latest_existing {
            None => panic!("Query refers to unknown view \"{}\"", view_name),
            Some(v) => {
                let existing = self.nodes.get(&(String::from(view_name), *v));
                match existing {
                    None => {
                        panic!(
                            "Inconsistency: view \"{}\" does not exist at v{}",
                            view_name, v
                        );
                    }
                    Some(bmn) => MirNode::reuse(bmn.clone(), self.schema_version),
                }
            }
        }
    }

    /// Converts a condition tree stored in the `ConditionExpr` returned by the SQL parser
    /// and adds its to a vector of conditions.
    fn to_conditions(
        &self,
        ct: &ConditionTree,
        columns: &mut Vec<Column>,
        n: &MirNodeRef,
    ) -> Vec<Option<FilterCondition>> {
        use std::cmp::max;

        // TODO(malte): we only support one level of condition nesting at this point :(
        let l = match *ct.left.as_ref() {
            ConditionExpression::Base(ConditionBase::Field(ref f)) => f.clone(),
            _ => unimplemented!(),
        };
        let f = Some(match *ct.right.as_ref() {
            ConditionExpression::Base(ConditionBase::Literal(Literal::Integer(ref i))) => {
                FilterCondition::Equality(ct.operator.clone(), DataType::from(*i))
            }
            ConditionExpression::Base(ConditionBase::Literal(Literal::String(ref s))) => {
                FilterCondition::Equality(ct.operator.clone(), DataType::from(s.clone()))
            }
            ConditionExpression::Base(ConditionBase::LiteralList(ref ll)) => {
                FilterCondition::In(ll.iter().map(|l| DataType::from(l.clone())).collect())
            }
            _ => unimplemented!(),
        });

        let absolute_column_ids: Vec<usize> = columns
            .iter()
            .map(|c| n.borrow().column_id_for_column(c))
            .collect();
        let max_column_id = *absolute_column_ids.iter().max().unwrap();
        let num_columns = max(columns.len(), max_column_id + 1);
        let mut filters = vec![None; num_columns];

        match columns.iter().rposition(|c| *c.name == l.name) {
            None => {
                // Might occur if the column doesn't exist in the parent; e.g., for aggregations.
                // We assume that the column is appended at the end.
                columns.push(l);
                filters.push(f);
            }
            Some(pos) => {
                filters[absolute_column_ids[pos]] = f;
            }
        }

        filters
    }

    pub fn add_leaf_below(
        &mut self,
        prior_leaf: MirNodeRef,
        name: &str,
        params: &Vec<Column>,
        project_columns: Option<Vec<Column>>,
    ) -> MirQuery {
        // hang off the previous logical leaf node
        let parent_columns: Vec<Column> = prior_leaf.borrow().columns().iter().cloned().collect();
        let parent = MirNode::reuse(prior_leaf, self.schema_version);

        let (reproject, columns): (bool, Vec<Column>) = match project_columns {
            // parent is a projection already, so no need to reproject; just reuse its columns
            None => (false, parent_columns),
            // parent is not a projection, so we need to reproject to the columns passed to us
            Some(pc) => (true, pc.into_iter().chain(params.iter().cloned()).collect()),
        };

        let n = if reproject {
            // add a (re-)projection and then another leaf
            MirNode::new(
                &format!("{}_reproject", name),
                self.schema_version,
                columns.clone(),
                MirNodeType::Project {
                    emit: columns.clone(),
                    literals: vec![],
                    arithmetic: vec![],
                },
                vec![parent.clone()],
                vec![],
            )
        } else {
            // add an identity node and then another leaf
            MirNode::new(
                &format!("{}_id", name),
                self.schema_version,
                columns.clone(),
                MirNodeType::Identity,
                vec![parent.clone()],
                vec![],
            )
        };

        let new_leaf = MirNode::new(
            name,
            self.schema_version,
            columns
                .clone()
                .into_iter()
                .map(|c| sanitize_leaf_column(c, name))
                .collect(),
            MirNodeType::Leaf {
                node: parent.clone(),
                keys: params.clone(),
            },
            vec![n],
            vec![],
        );

        // always register leaves
        self.current.insert(String::from(name), self.schema_version);
        self.nodes
            .insert((String::from(name), self.schema_version), new_leaf.clone());

        // wrap in a (very short) query to return
        MirQuery {
            name: String::from(name),
            roots: vec![parent],
            leaf: new_leaf,
        }
    }

    pub fn compound_query_to_mir(
        &mut self,
        name: &str,
        sqs: Vec<&MirQuery>,
        op: CompoundSelectOperator,
        order: &Option<OrderClause>,
        limit: &Option<LimitClause>,
        has_leaf: bool,
    ) -> MirQuery {
        let union_name = if !has_leaf && limit.is_none() {
            String::from(name)
        } else {
            format!("{}_union", name)
        };
        let mut final_node = match op {
            CompoundSelectOperator::Union => {
                self.make_union_node(&union_name, &sqs.iter().map(|mq| mq.leaf.clone()).collect())
            }
            _ => unimplemented!(),
        };
        let node_id = (union_name, self.schema_version);
        if !self.nodes.contains_key(&node_id) {
            self.nodes.insert(node_id, final_node.clone());
        }

        // we use these columns for intermediate nodes
        let columns: Vec<Column> = final_node.borrow().columns().iter().cloned().collect();
        // we use these columns for whichever node ends up being the leaf
        let sanitized_columns: Vec<Column> = columns
            .clone()
            .into_iter()
            .map(|c| sanitize_leaf_column(c, name))
            .collect();

        if limit.is_some() {
            let (topk_name, topk_columns) = if !has_leaf {
                (String::from(name), sanitized_columns.iter().collect())
            } else {
                (format!("{}_topk", name), columns.iter().collect())
            };
            let topk_node = self.make_topk_node(
                &topk_name,
                final_node,
                topk_columns,
                order,
                limit.as_ref().unwrap(),
            );
            let node_id = (topk_name, self.schema_version);
            if !self.nodes.contains_key(&node_id) {
                self.nodes.insert(node_id, topk_node.clone());
            }
            final_node = topk_node;
        }

        let leaf_node = if has_leaf {
            MirNode::new(
                name,
                self.schema_version,
                sanitized_columns,
                MirNodeType::Leaf {
                    node: final_node.clone(),
                    keys: vec![],
                },
                vec![final_node.clone()],
                vec![],
            )
        } else {
            final_node
        };

        self.current
            .insert(String::from(leaf_node.borrow().name()), self.schema_version);
        let node_id = (String::from(name), self.schema_version);
        if !self.nodes.contains_key(&node_id) {
            self.nodes.insert(node_id, leaf_node.clone());
        }

        MirQuery {
            name: String::from(name),
            roots: sqs.iter().fold(Vec::new(), |mut acc, mq| {
                acc.extend(mq.roots.iter().cloned().collect::<Vec<MirNodeRef>>());
                acc
            }),
            leaf: leaf_node,
        }
    }

    pub fn get_flow_node_address(&self, name: &str, version: usize) -> Option<NodeIndex> {
        match self.nodes.get(&(name.to_string(), version)) {
            None => None,
            Some(ref node) => match node.borrow().flow_node {
                None => None,
                Some(ref flow_node) => Some(flow_node.address()),
            },
        }
    }

    pub fn get_leaf(&self, name: &str) -> Option<NodeIndex> {
        match self.current.get(name) {
            None => None,
            Some(v) => self.get_flow_node_address(name, *v),
        }
    }

    pub fn named_base_to_mir(
        &mut self,
        name: &str,
        query: &SqlQuery,
        transactional: bool,
    ) -> MirQuery {
        match *query {
            SqlQuery::CreateTable(ref ctq) => {
                assert_eq!(name, ctq.table.name);
                let n = self.make_base_node(&name, &ctq.fields, ctq.keys.as_ref(), transactional);
                let node_id = (String::from(name), self.schema_version);
                if !self.nodes.contains_key(&node_id) {
                    self.nodes.insert(node_id, n.clone());
                    self.current.insert(String::from(name), self.schema_version);
                }
                MirQuery::singleton(name, n)
            }
            _ => panic!("expected CREATE TABLE query!"),
        }
    }

    pub fn named_query_to_mir(
        &mut self,
        name: &str,
        sq: &SelectStatement,
        qg: &QueryGraph,
        has_leaf: bool,
        universe: UniverseId,
    ) -> MirQuery {
        let nodes = self.make_nodes_for_selection(&name, sq, qg, has_leaf, universe);
        let mut roots = Vec::new();
        let mut leaves = Vec::new();
        for mn in nodes.into_iter() {
            let node_id = (String::from(mn.borrow().name()), self.schema_version);
            // only add the node if we don't have it registered at this schema version already. If
            // we don't do this, we end up adding the node again for every re-use of it, with
            // increasingly deeper chains of nested `MirNode::Reuse` structures.
            if !self.nodes.contains_key(&node_id) {
                self.nodes.insert(node_id, mn.clone());
            }

            if mn.borrow().ancestors().len() == 0 {
                // root
                roots.push(mn.clone());
            }
            if mn.borrow().children().len() == 0 {
                // leaf
                debug!(self.log, "node {:?} is a leaf", mn);
                leaves.push(mn);
            }
        }
        assert_eq!(
            leaves.len(),
            1,
            "expected just one leaf! leaves: {:?}",
            leaves
        );
        let leaf = leaves.into_iter().next().unwrap();
        self.current
            .insert(String::from(leaf.borrow().name()), self.schema_version);

        MirQuery {
            name: String::from(name),
            roots: roots,
            leaf: leaf,
        }
    }

    pub fn upgrade_schema(&mut self, new_version: usize) {
        assert!(new_version > self.schema_version);
        self.schema_version = new_version;
    }

    fn make_base_node(
        &mut self,
        name: &str,
        cols: &Vec<ColumnSpecification>,
        keys: Option<&Vec<TableKey>>,
        transactional: bool,
    ) -> MirNodeRef {
        // have we seen a base of this name before?
        if self.base_schemas.contains_key(name) {
            let mut existing_schemas: Vec<(usize, Vec<ColumnSpecification>)> =
                self.base_schemas[name].clone();
            existing_schemas.sort_by_key(|&(sv, _)| sv);
            // newest schema first
            existing_schemas.reverse();

            for (existing_sv, ref schema) in existing_schemas {
                // TODO(malte): check the keys too
                if schema == cols {
                    // exact match, so reuse the existing base node
                    info!(
                        self.log,
                        "base table for {} already exists with identical \
                         schema in version {}; reusing it.",
                        name,
                        existing_sv
                    );
                    let existing_node = self.nodes[&(String::from(name), existing_sv)].clone();
                    return MirNode::reuse(existing_node, self.schema_version);
                } else {
                    // match, but schema is different, so we'll need to either:
                    //  1) reuse the existing node, but add an upgrader for any changes in the
                    //     column set, or
                    //  2) give up and just make a new node
                    info!(
                        self.log,
                        "base table for {} already exists in version {}, \
                         but has a different schema!",
                        name,
                        existing_sv
                    );

                    // Find out if this is a simple case of adding or removing a column
                    let mut columns_added = Vec::new();
                    let mut columns_removed = Vec::new();
                    let mut columns_unchanged = Vec::new();
                    for c in cols {
                        if !schema.contains(c) {
                            // new column
                            columns_added.push(c);
                        } else {
                            columns_unchanged.push(c);
                        }
                    }
                    for c in schema {
                        if !cols.contains(c) {
                            // dropped column
                            columns_removed.push(c);
                        }
                    }

                    if columns_unchanged.len() > 0
                        && (columns_added.len() > 0 || columns_removed.len() > 0)
                    {
                        error!(
                            self.log,
                            "base {}: add columns {:?}, remove columns {:?} over v{}",
                            name,
                            columns_added,
                            columns_removed,
                            existing_sv
                        );
                        let existing_node = self.nodes[&(String::from(name), existing_sv)].clone();

                        let mut columns: Vec<ColumnSpecification> = existing_node
                            .borrow()
                            .column_specifications()
                            .iter()
                            .map(|&(ref cs, _)| cs.clone())
                            .collect();
                        for added in &columns_added {
                            columns.push((*added).clone());
                        }
                        for removed in &columns_removed {
                            let pos = columns.iter().position(|cc| cc == *removed).expect(
                                &format!(
                                    "couldn't find column \"{:#?}\", \
                                     which we're removing",
                                    removed
                                ),
                            );
                            columns.remove(pos);
                        }
                        assert_eq!(
                            columns.len(),
                            existing_node.borrow().columns().len() + columns_added.len()
                                - columns_removed.len()
                        );

                        // remember the schema for this version
                        let base_schemas = self.base_schemas.entry(String::from(name)).or_default();
                        base_schemas.push((self.schema_version, columns.clone()));

                        return MirNode::adapt_base(existing_node, columns_added, columns_removed);
                    } else {
                        info!(self.log, "base table has complex schema change");
                        break;
                    }
                }
            }
        }

        // all columns on a base must have the base as their table
        assert!(
            cols.iter()
                .all(|c| c.column.table == Some(String::from(name)))
        );

        let primary_keys = match keys {
            None => vec![],
            Some(keys) => keys.iter()
                .filter_map(|k| match *k {
                    ref k @ TableKey::PrimaryKey(..) => Some(k),
                    _ => None,
                })
                .collect(),
        };
        // TODO(malte): support >1 pkey
        assert!(primary_keys.len() <= 1);

        // remember the schema for this version
        let base_schemas = self.base_schemas.entry(String::from(name)).or_default();
        base_schemas.push((self.schema_version, cols.clone()));

        // make node
        if !primary_keys.is_empty() {
            match **primary_keys.iter().next().unwrap() {
                TableKey::PrimaryKey(ref key_cols) => {
                    debug!(
                        self.log,
                        "Assigning primary key ({}) for base {}",
                        key_cols
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        name
                    );
                    MirNode::new(
                        name,
                        self.schema_version,
                        cols.iter().map(|cs| cs.column.clone()).collect(),
                        MirNodeType::Base {
                            column_specs: cols.iter().map(|cs| (cs.clone(), None)).collect(),
                            keys: key_cols.clone(),
                            transactional,
                            adapted_over: None,
                        },
                        vec![],
                        vec![],
                    )
                }
                _ => unreachable!(),
            }
        } else {
            MirNode::new(
                name,
                self.schema_version,
                cols.iter().map(|cs| cs.column.clone()).collect(),
                MirNodeType::Base {
                    column_specs: cols.iter().map(|cs| (cs.clone(), None)).collect(),
                    keys: vec![],
                    transactional,
                    adapted_over: None,
                },
                vec![],
                vec![],
            )
        }
    }

    fn make_union_node(&self, name: &str, ancestors: &Vec<MirNodeRef>) -> MirNodeRef {
        let mut emit: Vec<Vec<Column>> = Vec::new();
        assert!(ancestors.len() > 1, "union must have more than 1 ancestors");

        let ucols: Vec<Column> = ancestors
            .first()
            .unwrap()
            .borrow()
            .columns()
            .iter()
            .cloned()
            .collect();

        // Find columns present in all ancestors
        let mut selected_cols = HashSet::new();
        for c in ucols {
            if ancestors
                .iter()
                .all(|a| a.borrow().columns().iter().any(|ac| ac.name == c.name))
            {
                selected_cols.insert(c.name.clone());
            }
        }

        for ancestor in ancestors.iter() {
            let mut acols: Vec<Column> = Vec::new();
            for ac in ancestor.borrow().columns() {
                if selected_cols.contains(&ac.name)
                    && acols.iter().find(|ref c| ac.name == c.name).is_none()
                {
                    acols.push(ac.clone());
                }
            }
            emit.push(acols.clone());
        }

        assert!(
            emit.iter().all(|e| e.len() == selected_cols.len()),
            "all ancestors columns must have the same size"
        );

        MirNode::new(
            name,
            self.schema_version,
            emit.first().unwrap().clone(),
            MirNodeType::Union { emit },
            ancestors.clone(),
            vec![],
        )
    }

    fn make_union_from_same_base(
        &self,
        name: &str,
        ancestors: Vec<MirNodeRef>,
        columns: Vec<Column>,
    ) -> MirNodeRef {
        assert!(ancestors.len() > 1, "union must have more than 1 ancestors");
        trace!(self.log, "Added union node wiht columns {:?}", columns);
        let emit = ancestors.iter().map(|_| columns.clone()).collect();

        MirNode::new(
            name,
            self.schema_version,
            columns,
            MirNodeType::Union { emit },
            ancestors.clone(),
            vec![],
        )
    }

    fn make_filter_node(&self, name: &str, parent: MirNodeRef, cond: &ConditionTree) -> MirNodeRef {
        let mut fields = parent.borrow().columns().iter().cloned().collect();

        let filter = self.to_conditions(cond, &mut fields, &parent);
        trace!(
            self.log,
            "Added filter node {} with condition {:?}",
            name,
            filter
        );
        MirNode::new(
            name,
            self.schema_version,
            fields,
            MirNodeType::Filter { conditions: filter },
            vec![parent.clone()],
            vec![],
        )
    }

    fn make_function_node(
        &mut self,
        name: &str,
        func_col: &Column,
        group_cols: Vec<&Column>,
        parent: MirNodeRef,
    ) -> MirNodeRef {
        use dataflow::ops::grouped::aggregate::Aggregation;
        use dataflow::ops::grouped::extremum::Extremum;
        use nom_sql::FunctionExpression::*;

        let mknode = |over: &Column, t: GroupedNodeType| {
            self.make_grouped_node(name, &func_col, (parent, &over), group_cols, t)
        };

        let func = func_col.function.as_ref().unwrap();
        match *func.deref() {
            Sum(ref col, _) => mknode(col, GroupedNodeType::Aggregation(Aggregation::SUM)),
            Count(ref col, _) => mknode(col, GroupedNodeType::Aggregation(Aggregation::COUNT)),
            CountStar => {
                // XXX(malte): there is no "over" column, but our aggregation operators' API
                // requires one to be specified, so we earlier rewrote it to use the last parent
                // column (see passes/count_star_rewrite.rs). However, this isn't *entirely*
                // faithful to COUNT(*) semantics, because COUNT(*) is supposed to count all
                // rows including those with NULL values, and we don't have a mechanism to do that
                // (but we also don't have a NULL value, so maybe we're okay).
                panic!("COUNT(*) should have been rewritten earlier!")
            }
            Max(ref col) => mknode(col, GroupedNodeType::Extremum(Extremum::MAX)),
            Min(ref col) => mknode(col, GroupedNodeType::Extremum(Extremum::MIN)),
            GroupConcat(ref col, ref separator) => {
                mknode(col, GroupedNodeType::GroupConcat(separator.clone()))
            }
            _ => unimplemented!(),
        }
    }

    fn make_grouped_node(
        &mut self,
        name: &str,
        computed_col: &Column,
        over: (MirNodeRef, &Column),
        group_by: Vec<&Column>,
        node_type: GroupedNodeType,
    ) -> MirNodeRef {
        let parent_node = over.0;

        // Resolve column IDs in parent
        let over_col = over.1;

        // move alias to name in computed column (which needs not to
        // match against a parent node column, and is often aliased)
        let computed_col = match computed_col.alias {
            None => computed_col.clone(),
            Some(ref a) => Column {
                name: a.clone(),
                alias: None,
                table: computed_col.table.clone(),
                function: computed_col.function.clone(),
            },
        };

        // The function node's set of output columns is the group columns plus the function
        // column
        let mut combined_columns = group_by
            .iter()
            .map(|c| (*c).clone())
            .collect::<Vec<Column>>();
        combined_columns.push(computed_col.clone());

        // make the new operator
        match node_type {
            GroupedNodeType::Aggregation(agg) => MirNode::new(
                name,
                self.schema_version,
                combined_columns,
                MirNodeType::Aggregation {
                    on: over_col.clone(),
                    group_by: group_by.into_iter().cloned().collect(),
                    kind: agg,
                },
                vec![parent_node.clone()],
                vec![],
            ),
            GroupedNodeType::Extremum(extr) => MirNode::new(
                name,
                self.schema_version,
                combined_columns,
                MirNodeType::Extremum {
                    on: over_col.clone(),
                    group_by: group_by.into_iter().cloned().collect(),
                    kind: extr,
                },
                vec![parent_node.clone()],
                vec![],
            ),
            GroupedNodeType::GroupConcat(sep) => MirNode::new(
                name,
                self.schema_version,
                combined_columns,
                MirNodeType::GroupConcat {
                    on: over_col.clone(),
                    separator: sep,
                },
                vec![parent_node.clone()],
                vec![],
            ),
        }
    }

    fn make_join_node(
        &self,
        name: &str,
        jp: &ConditionTree,
        left_node: MirNodeRef,
        right_node: MirNodeRef,
        kind: JoinType,
    ) -> MirNodeRef {
        let projected_cols_left = left_node
            .borrow()
            .columns()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let projected_cols_right = right_node
            .borrow()
            .columns()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let fields = projected_cols_left
            .into_iter()
            .chain(projected_cols_right.into_iter())
            .collect::<Vec<Column>>();

        // join columns need us to generate join group configs for the operator
        // TODO(malte): no multi-level joins yet
        let mut left_join_columns = Vec::new();
        let mut right_join_columns = Vec::new();

        // equi-join only
        assert!(jp.operator == Operator::Equal || jp.operator == Operator::In);
        let l_col = match *jp.left {
            ConditionExpression::Base(ConditionBase::Field(ref f)) => f.clone(),
            _ => unimplemented!(),
        };
        let r_col = match *jp.right {
            ConditionExpression::Base(ConditionBase::Field(ref f)) => f.clone(),
            _ => unimplemented!(),
        };
        left_join_columns.push(l_col);
        right_join_columns.push(r_col);

        assert_eq!(left_join_columns.len(), right_join_columns.len());
        let inner = match kind {
            JoinType::Inner => MirNodeType::Join {
                on_left: left_join_columns,
                on_right: right_join_columns,
                project: fields.clone(),
            },
            JoinType::Left => MirNodeType::LeftJoin {
                on_left: left_join_columns,
                on_right: right_join_columns,
                project: fields.clone(),
            },
        };
        trace!(self.log, "Added join node {:?}", inner);
        MirNode::new(
            name,
            self.schema_version,
            fields,
            inner,
            vec![left_node.clone(), right_node.clone()],
            vec![],
        )
    }

    fn make_projection_helper(
        &mut self,
        name: &str,
        parent: MirNodeRef,
        fn_col: &Column,
    ) -> MirNodeRef {
        self.make_project_node(
            name,
            parent,
            vec![fn_col],
            vec![],
            vec![(String::from("grp"), DataType::from(0 as i32))],
            false,
        )
    }

    fn make_project_node(
        &mut self,
        name: &str,
        parent_node: MirNodeRef,
        proj_cols: Vec<&Column>,
        arithmetic: Vec<(String, ArithmeticExpression)>,
        literals: Vec<(String, DataType)>,
        is_leaf: bool,
    ) -> MirNodeRef {
        //assert!(proj_cols.iter().all(|c| c.table == parent_name));

        let names: Vec<String> = literals
            .iter()
            .map(|&(ref n, _)| n.clone())
            .chain(arithmetic.iter().map(|&(ref n, _)| n.clone()))
            .collect();

        let fields = proj_cols
            .clone()
            .into_iter()
            .map(|c| match c.alias {
                Some(ref a) => Column {
                    name: a.clone(),
                    table: if is_leaf {
                        // if this is the leaf node of a query, it represents a view, so we rewrite
                        // the table name here.
                        Some(String::from(name))
                    } else {
                        c.table.clone()
                    },
                    alias: None,
                    function: c.function.clone(),
                },
                None => {
                    // if this is the leaf node of a query, it represents a view, so we rewrite the
                    // table name here.
                    if is_leaf {
                        sanitize_leaf_column(c.clone(), name)
                    } else {
                        c.clone()
                    }
                }
            })
            .chain(names.into_iter().map(|n| Column {
                name: n,
                alias: None,
                table: Some(String::from(name)),
                function: None,
            }))
            .collect();

        // remove aliases from emit columns because they are later compared to parent node columns
        // and need to be equal. Note that `fields`, which holds the column names applied,
        // preserves the aliases.
        let emit_cols = proj_cols
            .into_iter()
            .cloned()
            .map(|mut c| {
                match c.alias {
                    Some(_) => c.alias = None,
                    None => (),
                };
                c
            })
            .collect();

        MirNode::new(
            name,
            self.schema_version,
            fields,
            MirNodeType::Project {
                emit: emit_cols,
                literals: literals,
                arithmetic: arithmetic,
            },
            vec![parent_node.clone()],
            vec![],
        )
    }

    fn make_topk_node(
        &mut self,
        name: &str,
        parent: MirNodeRef,
        group_by: Vec<&Column>,
        order: &Option<OrderClause>,
        limit: &LimitClause,
    ) -> MirNodeRef {
        let combined_columns = parent.borrow().columns().iter().cloned().collect();

        let order = match *order {
            Some(ref o) => Some(o.columns.clone()),
            None => None,
        };

        assert_eq!(limit.offset, 0); // Non-zero offset not supported

        // make the new operator and record its metadata
        MirNode::new(
            name,
            self.schema_version,
            combined_columns,
            MirNodeType::TopK {
                order: order,
                group_by: group_by.into_iter().cloned().collect(),
                k: limit.limit as usize,
                offset: 0,
            },
            vec![parent.clone()],
            vec![],
        )
    }

    fn make_predicate_nodes(
        &self,
        name: &str,
        parent: MirNodeRef,
        ce: &ConditionExpression,
        nc: usize,
    ) -> Vec<MirNodeRef> {
        use nom_sql::ConditionExpression::*;

        let mut pred_nodes: Vec<MirNodeRef> = Vec::new();
        let output_cols = parent.borrow().columns().iter().cloned().collect();
        match *ce {
            LogicalOp(ref ct) => {
                let (left, right);
                match ct.operator {
                    Operator::And => {
                        left = self.make_predicate_nodes(name, parent.clone(), &*ct.left, nc);

                        right = self.make_predicate_nodes(
                            name,
                            left.last().unwrap().clone(),
                            &*ct.right,
                            nc + left.len(),
                        );

                        pred_nodes.extend(left.clone());
                        pred_nodes.extend(right.clone());
                    }
                    Operator::Or => {
                        left = self.make_predicate_nodes(name, parent.clone(), &*ct.left, nc);

                        right = self.make_predicate_nodes(
                            name,
                            parent.clone(),
                            &*ct.right,
                            nc + left.len(),
                        );

                        debug!(self.log, "Creating union node for `or` predicate");

                        let last_left = left.last().unwrap().clone();
                        let last_right = right.last().unwrap().clone();
                        let union = self.make_union_from_same_base(
                            &format!("{}_un", name),
                            vec![last_left, last_right],
                            output_cols,
                        );

                        pred_nodes.extend(left.clone());
                        pred_nodes.extend(right.clone());
                        pred_nodes.push(union);
                    }
                    _ => unreachable!("LogicalOp operator is {:?}", ct.operator),
                }
            }
            ComparisonOp(ref ct) => {
                // currently, we only support filter-like
                // comparison operations, no nested-selections
                let f = self.make_filter_node(&format!("{}_f{}", name, nc), parent, ct);

                pred_nodes.push(f);
            }
            NegationOp(_) => unreachable!("negation should have been removed earlier"),
            Base(_) => unreachable!("dangling base predicate"),
        }

        pred_nodes
    }

    /// Returns all collumns used in a predicate
    fn predicate_columns(&self, ce: ConditionExpression) -> HashSet<Column> {
        use nom_sql::ConditionExpression::*;

        let mut cols = HashSet::new();
        match ce {
            LogicalOp(ct) | ComparisonOp(ct) => {
                cols.extend(self.predicate_columns(*ct.left));
                cols.extend(self.predicate_columns(*ct.right));
            }
            Base(ConditionBase::Field(c)) => {
                cols.insert(c);
            }
            NegationOp(_) => unreachable!("negations should have been eliminated"),
            _ => (),
        }

        cols
    }

    fn predicates_above_group_by<'a>(
        &mut self,
        name: &str,
        column_to_predicates: &HashMap<Column, Vec<&'a ConditionExpression>>,
        over_col: Column,
        parent: MirNodeRef,
        created_predicates: &mut Vec<&'a ConditionExpression>,
    ) -> Vec<MirNodeRef> {
        let mut predicates_above_group_by_nodes = Vec::new();
        let mut prev_node = parent.clone();

        let ces = column_to_predicates.get(&over_col).unwrap();
        for ce in ces {
            if !created_predicates.contains(ce) {
                let mpns = self.make_predicate_nodes(
                    &format!("{}_mp{}", name, predicates_above_group_by_nodes.len()),
                    prev_node.clone(),
                    ce,
                    0,
                );
                assert!(mpns.len() > 0);
                prev_node = mpns.last().unwrap().clone();
                predicates_above_group_by_nodes.extend(mpns);
                created_predicates.push(ce);
            }
        }

        predicates_above_group_by_nodes
    }

    /// Returns list of nodes added
    fn make_nodes_for_selection(
        &mut self,
        name: &str,
        st: &SelectStatement,
        qg: &QueryGraph,
        has_leaf: bool,
        universe: UniverseId,
    ) -> Vec<MirNodeRef> {
        use std::collections::HashMap;
        use controller::sql::mir::join::make_joins;
        use controller::sql::mir::grouped::make_predicates_above_grouped;
        use controller::sql::mir::grouped::make_grouped;

        let mut nodes_added: Vec<MirNodeRef>;
        let mut new_node_count = 0;

        let (uid, _) = universe.clone();

        let uformat = if uid == "global".into() {
            String::from("")
        } else {
            format!("_u{}", uid.to_string())
        };

        // Canonical operator order: B-J-G-F-P-R
        // (Base, Join, GroupBy, Filter, Project, Reader)
        {
            let mut node_for_rel: HashMap<&str, MirNodeRef> = HashMap::default();

            // 0. Base nodes (always reused)
            let mut base_nodes: Vec<MirNodeRef> = Vec::new();
            let mut sorted_rels: Vec<&str> = qg.relations.keys().map(String::as_str).collect();
            sorted_rels.sort();
            for rel in &sorted_rels {
                if *rel == "computed_columns" {
                    continue;
                }

                let base_for_rel = self.get_view(rel);

                base_nodes.push(base_for_rel.clone());
                node_for_rel.insert(*rel, base_for_rel);
            }

            let join_nodes = make_joins(
                self,
                &format!("q_{:x}{}", qg.signature().hash, uformat),
                qg,
                &node_for_rel,
                new_node_count,
            );

            new_node_count += join_nodes.len();

            let mut prev_node = match join_nodes.last() {
                Some(n) => Some(n.clone()),
                None => {
                    assert_eq!(base_nodes.len(), 1);
                    Some(base_nodes.last().unwrap().clone())
                }
            };

            // 2. Get columns used by each predicate. This will be used to check
            // if we need to reorder predicates before group_by nodes.
            let mut column_to_predicates: HashMap<Column, Vec<&ConditionExpression>> =
                HashMap::new();

            for rel in &sorted_rels {
                if *rel == "computed_columns" {
                    continue;
                }

                let qgn = &qg.relations[*rel];
                for pred in &qgn.predicates {
                    let cols = self.predicate_columns(pred.clone());

                    for col in cols {
                        column_to_predicates.entry(col).or_default().push(pred);
                    }
                }
            }

            // 2.5 Reorder some predicates before group by nodes
            let (created_predicates, predicates_above_group_by_nodes) =
                make_predicates_above_grouped(
                    self,
                    &format!("q_{:x}{}", qg.signature().hash, uformat),
                    &qg,
                    &node_for_rel,
                    new_node_count,
                    &column_to_predicates,
                    &mut prev_node,
                );

            new_node_count += predicates_above_group_by_nodes.len();

            // Create security boundary
            use controller::sql::mir::security::SecurityBoundary;
            let (last_policy_nodes, policy_nodes) =
                self.make_security_boundary(universe.clone(), &mut node_for_rel, prev_node.clone());

            let mut ancestors = self.universe.member_of.iter().fold(
                vec![],
                |mut acc, (gname, gids)| {
                    let group_views: Vec<MirNodeRef> = gids.iter()
                        .filter_map(|gid| {
                            // This is a little annoying, but because of the way we name universe queries,
                            // we need to strip the view name of the _u{uid} suffix
                            let root = name.trim_right_matches(&uformat);
                            if root == name {
                                None
                            } else {
                                let view_name =
                                    format!("{}_{}{}", root, gname.to_string(), gid.to_string());
                                Some(self.get_view(&view_name))
                            }
                        })
                        .collect();

                    trace!(self.log, "group views {:?}", group_views);
                    acc.extend(group_views);
                    acc
                },
            );

            nodes_added = base_nodes
                .into_iter()
                .chain(join_nodes.into_iter())
                .chain(predicates_above_group_by_nodes.into_iter())
                .chain(policy_nodes.into_iter())
                .chain(ancestors.clone().into_iter())
                .collect();

            // For each policy chain, create a version of the query
            // All query versions, including group queries will be reconciled at the end
            for n in last_policy_nodes.iter() {
                prev_node = Some(n.clone());

                // 3. Add function and grouped nodes
                let mut func_nodes: Vec<MirNodeRef> = make_grouped(
                    self,
                    &format!("q_{:x}{}", qg.signature().hash, uformat),
                    &qg,
                    &node_for_rel,
                    new_node_count,
                    &mut prev_node,
                    false,
                );

                new_node_count += func_nodes.len();

                let mut predicate_nodes = Vec::new();
                // 4. Generate the necessary filter node for each relation node in the query graph.

                // Need to iterate over relations in a deterministic order, as otherwise nodes will be
                // added in a different order every time, which will yield different node identifiers
                // and make it difficult for applications to check what's going on.
                for rel in &sorted_rels {
                    let qgn = &qg.relations[*rel];
                    // we've already handled computed columns
                    if *rel == "computed_columns" {
                        continue;
                    }

                    // the following conditional is required to avoid "empty" nodes (without any
                    // projected columns) that are required as inputs to joins
                    if !qgn.predicates.is_empty() {
                        // add a predicate chain for each query graph node's predicates
                        for (i, ref p) in qgn.predicates.iter().enumerate() {
                            if created_predicates.contains(p) {
                                continue;
                            }

                            let parent = match prev_node {
                                None => node_for_rel[rel].clone(),
                                Some(pn) => pn,
                            };

                            let fns = self.make_predicate_nodes(
                                &format!(
                                    "q_{:x}_n{}_p{}{}",
                                    qg.signature().hash,
                                    new_node_count,
                                    i,
                                    uformat
                                ),
                                parent,
                                p,
                                0,
                            );

                            assert!(fns.len() > 0);
                            new_node_count += fns.len();
                            prev_node = Some(fns.iter().last().unwrap().clone());
                            predicate_nodes.extend(fns);
                        }
                    }
                }

                // 5. Get the final node
                let mut final_node: MirNodeRef = if prev_node.is_some() {
                    prev_node.unwrap().clone()
                } else {
                    // no join, filter, or function node --> base node is parent
                    assert_eq!(sorted_rels.len(), 1);
                    node_for_rel[sorted_rels.last().unwrap()].clone()
                };

                // 6. Potentially insert TopK node below the final node
                if let Some(ref limit) = st.limit {
                    let group_by = qg.parameters();

                    let node = self.make_topk_node(
                        &format!("q_{:x}_n{}{}", qg.signature().hash, new_node_count, uformat),
                        final_node,
                        group_by,
                        &st.order,
                        limit,
                    );
                    func_nodes.push(node.clone());
                    final_node = node;
                    new_node_count += 1;
                }

                // we're now done with the query, so remember all the nodes we've added so far
                nodes_added.extend(func_nodes);
                nodes_added.extend(predicate_nodes);

                ancestors.push(final_node);
            }

            let final_node = if ancestors.len() > 1 {
                // If we have multiple queries, reconcile them.
                let nodes = self.reconcile(
                    &format!("q_{:x}{}", qg.signature().hash, uformat),
                    &qg,
                    &ancestors,
                    new_node_count,
                );
                new_node_count += nodes.len();
                nodes_added.extend(nodes.clone());

                nodes.last().unwrap().clone()
            } else {
                ancestors.last().unwrap().clone()
            };

            let final_node_cols: Vec<Column> =
                final_node.borrow().columns().iter().cloned().collect();
            // 5. Generate leaf views that expose the query result
            let mut projected_columns: Vec<&Column> = if universe.1.is_none() {
                qg.columns
                    .iter()
                    .filter_map(|oc| match *oc {
                        OutputColumn::Arithmetic(_) => None,
                        OutputColumn::Data(ref c) => Some(c),
                        OutputColumn::Literal(_) => None,
                    })
                    .collect()
            } else {
                // If we are creating a query for a group universe, we project
                // all columns in the final node. When a user universe that
                // belongs to this group, the proper projection and leaf node
                // will be added.
                final_node_cols.iter().collect()
            };

            for pc in qg.parameters() {
                if !projected_columns.contains(&pc) {
                    projected_columns.push(pc);
                }
            }
            let projected_arithmetic: Vec<(String, ArithmeticExpression)> = qg.columns
                .iter()
                .filter_map(|oc| match *oc {
                    OutputColumn::Arithmetic(ref ac) => {
                        Some((ac.name.clone(), ac.expression.clone()))
                    }
                    OutputColumn::Data(_) => None,
                    OutputColumn::Literal(_) => None,
                })
                .collect();
            let mut projected_literals: Vec<(String, DataType)> = qg.columns
                .iter()
                .filter_map(|oc| match *oc {
                    OutputColumn::Arithmetic(_) => None,
                    OutputColumn::Data(_) => None,
                    OutputColumn::Literal(ref lc) => {
                        Some((lc.name.clone(), DataType::from(&lc.value)))
                    }
                })
                .collect();

            // if this query does not have any parameters, we must add a bogokey
            let has_bogokey = if has_leaf && qg.parameters().is_empty() {
                projected_literals.push(("bogokey".into(), DataType::from(0 as i32)));
                true
            } else {
                false
            };

            let ident = if has_leaf {
                format!("q_{:x}_n{}{}", qg.signature().hash, new_node_count, uformat)
            } else {
                String::from(name)
            };

            let leaf_project_node = self.make_project_node(
                &ident,
                final_node,
                projected_columns,
                projected_arithmetic,
                projected_literals,
                !has_leaf,
            );

            nodes_added.push(leaf_project_node.clone());

            if has_leaf {
                // We are supposed to add a `MaterializedLeaf` node keyed on the query
                // parameters. For purely internal views (e.g., subqueries), this is not set.
                let columns = leaf_project_node
                    .borrow()
                    .columns()
                    .iter()
                    .cloned()
                    .map(|c| sanitize_leaf_column(c, name))
                    .collect();

                let query_params = if has_bogokey {
                    vec![
                        Column {
                            name: String::from("bogokey"),
                            alias: None,
                            table: Some(ident.clone()),
                            function: None,
                        },
                    ]
                } else {
                    qg.parameters().into_iter().cloned().collect()
                };

                let leaf_node = MirNode::new(
                    name,
                    self.schema_version,
                    columns,
                    MirNodeType::Leaf {
                        node: leaf_project_node.clone(),
                        keys: query_params,
                    },
                    vec![leaf_project_node.clone()],
                    vec![],
                );
                nodes_added.push(leaf_node);
            }

            debug!(
                self.log,
                "Added final MIR node for query named \"{}\"", name
            );
        }

        // finally, we output all the nodes we generated
        nodes_added
    }
}
