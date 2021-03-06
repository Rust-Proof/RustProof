// The Rust-Proof Project is copyright 2016, Sami Sahli,
// Michael Salter, Matthew Slocum, Vincent Schuster,
// Bradley Rasmussen, Drew Gohman, and Matthew O'Brien.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Generates a weakest precondition from MIR basic block data.

extern crate rustc_const_math;

use super::MirData;
use expression::*;
use rustc::mir::repr::*;
use rustc::middle::const_val::ConstVal;
use rustc_const_math::ConstInt;
use rustc_data_structures::indexed_vec::Idx;
use rustc::ty::{TypeVariants};

mod overflow;

/// Computes the weakest precondition for a given postcondition and a series of statements over one or more MIR basic blocks.
///
/// # Arguments:
/// * `index` - The index of the `BasicBlock` within MIR.
/// * `data` - Contains the `BasicBlockData` and all argument, temp, and variable declarations from the MIR pass.
/// * `post_expr` - The current weakest precondition (originally the postcondition) as an Expression.
///
/// # Return Value:
/// * Returns the weakest precondition generated from the `BasicBlock` in the form of an Expression.
///
/// # Remarks:
/// * This is the main generator for the weakest precondition, which evaluates the `BasicBlock`s recursively.
///
pub fn gen(index: usize, data: &mut MirData, post_expr: &Option<Expression>, debug: bool) -> Option<Expression> {
    let mut wp: Option<Expression>;

    // Parse basic block terminator data
    let terminator = data.block_data[index].terminator.clone().unwrap().kind;
    match terminator {
        // Assert{cond, expected, msg, target, cleanup}
        TerminatorKind::Assert{target, ..}
        | TerminatorKind::Goto{target} => {
            // Retrieve the weakest precondition from the following block
            wp = gen(target.index(), data, post_expr, debug);
        },
        TerminatorKind::Return => {
            // Return the post condition to the preceeding block
            return post_expr.clone();
        },
        // Call{func, args, destination, cleanup}
        TerminatorKind::Call{func, ..} => {
            // Determine if this is the end of a panic. (assumed false branch of assertion, so
            // return a precondition of false [this path will never be taken])
            match func {
                Operand::Constant (ref c) => {
                    let s = format!("{:?}", c.literal);
                    if s.contains("begin_panic") {
                        return Some(Expression::BooleanLiteral(false));
                    }
                },
                // Consume (ref l)
                Operand::Consume (..) => unimplemented!(),
            };
            // Due to the limited nature in which we handle Calls, we should never do anything
            // other than return early or hit the unimplemented!() panic above.
            unreachable!();
        },
        // Conditional statements
        // wp(if c x else y) => (c -> x) AND ((NOT c) -> y)
        TerminatorKind::If{cond, targets} => {
            // Generate weakest precondition for if and else clause
            let wp_if = gen(targets.0.index(), data, post_expr, debug);
            let wp_else = gen(targets.1.index(), data, post_expr, debug);

            // Generate the conditional expression
            let condition = match cond {
                Operand::Constant (ref constant) => {
                    match constant.literal {
                        Literal::Value {ref value} => {
                            match *value {
                                ConstVal::Bool (ref boolean) => {
                                    Expression::BooleanLiteral(*boolean)
                                },
                                _ => unreachable!(),
                            }
                        },
                        _ => unimplemented!(),
                    }
                },
                Operand::Consume(c) => { Expression::VariableMapping(gen_lvalue(c, data)) },
            };
            // Negate the conditional expression
            let not_condition = Expression::UnaryExpression(UnaryExpressionData {
                op: UnaryOperator::Not,
                e: Box::new(condition.clone())
            });
            // wp(If c x else y) => (c -> x) AND ((NOT c) -> y)
            wp = Some(Expression::BinaryExpression(BinaryExpressionData {
                op: BinaryOperator::And,
                left: Box::new(Expression::BinaryExpression(BinaryExpressionData {
                    op: BinaryOperator::Implication,
                    left: Box::new(condition.clone()),
                    right: Box::new(wp_if.unwrap())
                })),
                right: Box::new(Expression::BinaryExpression(BinaryExpressionData {
                    op: BinaryOperator::Implication,
                    left: Box::new(not_condition.clone()),
                    right: Box::new(wp_else.unwrap())
                }))
            }));
        },
        // Unimplemented TerminatorKinds
        // DropAndReplace{location, value, target, unwind}
        TerminatorKind::DropAndReplace{..} => unimplemented!(),
        // Drop{location, target, unwind}
        TerminatorKind::Drop{..} => unimplemented!(),
        TerminatorKind::Unreachable => unimplemented!(),
        TerminatorKind::Resume => unimplemented!(),
        // Switch{discr, adt_def, targets}
        TerminatorKind::Switch{..} => unimplemented!(),
        // SwitchInt{discr, switch_ty, values, targets}
        TerminatorKind::SwitchInt{..} => unimplemented!(),
    }

    // Examine the statements in reverse order
    let mut stmts = data.block_data[index].statements.clone();
    stmts.reverse();

    // Prints the current BasicBlock index
    if debug {
        println!("Processing bb{:?}:", index);
    }

    for stmt in stmts {
        // Modify the weakest precondition based on the statement
        wp = gen_stmt(wp.unwrap(), stmt, data, debug);
    }

    // Prints the result to be returned to the proceeding block
    if debug {
        println!("wp returned as\t{:?}\n", wp.clone().unwrap());
    }

    // Return the weakest precondition to the preceeding block, or to control
    wp
}

/// Returns a (possibly) modified weakest precondition based on the content of a statement
///
/// # Arguments:
/// * `wp` - The current weakest precondition
/// * `stmt` - The statement to be processed.
/// * `data` - Contains the `BasicBlockData` and all argument, temp, and variable declarations from
///            the MIR pass.
///
/// # Return Value:
/// * Returns the modified weakest precondition with underflow check
///
/// # Remarks:
///
fn gen_stmt(mut wp: Expression, stmt: Statement, data: &mut MirData, debug: bool)
            -> Option<Expression>  {
    // Prints the current statement being processed.
    if debug {
        println!("processing statement\t{:?}\ninto expression\t\t{:?}", stmt, wp);
    }

    let lvalue: Option<Lvalue>;
    let rvalue: Option<Rvalue>;

    // Store the values of the statement
    match stmt.kind {
        StatementKind::Assign(ref lval, ref rval) => {
            lvalue = Some(lval.clone());
            rvalue = Some(rval.clone());
        },
        //_ => return Some(wp)
    }
    // The variable or temp on the left-hand side of the assignment
    let mut var = gen_lvalue(lvalue.unwrap(), data);

    // The expression on the right-hand side of the assignment
    let mut expression = Vec::new();
    match rvalue.clone().unwrap() {
        Rvalue::CheckedBinaryOp(ref binop, ref loperand, ref roperand) => {
            let lvalue: Expression = gen_expression(loperand, data);
            let rvalue: Expression = gen_expression(roperand, data);
            let op: BinaryOperator = match *binop {
                BinOp::Add => {
                    // Add the overflow expression checks
                    wp = overflow::overflow_check(&wp, &var, binop, &lvalue, &rvalue);
                    BinaryOperator::Addition
                },
                BinOp::Sub => {
                    // Add the overflow and underflow expression checks
                    wp = overflow::overflow_check(&wp, &var, binop, &lvalue, &rvalue);
                    BinaryOperator::Subtraction
                },
                BinOp::Mul => {
                    // Add the overflow and underflow expression checks
                    wp = overflow::overflow_check(&wp, &var, binop, &lvalue, &rvalue);
                    BinaryOperator::Multiplication
                },
                BinOp::Div => {
                    // Add the overflow and underflow expression checks, if operands are signed
                    if is_signed_type(determine_evaluation_type(&rvalue)) {
                        wp = overflow::overflow_check(&wp, &var, binop, &lvalue, &rvalue);
                    }
                    // Add the division by 0 expression check
                    wp = add_zero_check(&wp, &rvalue);
                    BinaryOperator::Division
                },
                BinOp::Rem => {
                    // Add the overflow and underflow expression checks, if operands are signed
                    if is_signed_type(determine_evaluation_type(&rvalue)) {
                        wp = overflow::overflow_check(&wp, &var, binop, &lvalue, &rvalue);
                    }
                    // Add the division by 0 expression check
                    wp = add_zero_check(&wp, &rvalue);
                    BinaryOperator::Modulo
                },
                BinOp::Shl => BinaryOperator::BitwiseLeftShift,
                BinOp::Shr => BinaryOperator::BitwiseRightShift,
                _ => rp_error!("Unsupported checked binary operation!"),
            };

            var.name = var.name + ".0";

            // Add the new BinaryExpressionData to the expression vector
            expression.push(Expression::BinaryExpression( BinaryExpressionData {
                op: op,
                left: Box::new(lvalue),
                right: Box::new(rvalue)
            } ));
        },

        Rvalue::BinaryOp(ref binop, ref lval, ref rval) => {
            let lvalue: Expression = gen_expression(lval, data);
            let rvalue: Expression = gen_expression(rval, data);
            let op: BinaryOperator = match *binop {
                BinOp::Add => {
                    // Add the overflow expression check
                    wp = overflow::overflow_check(&wp, &var, binop, &lvalue, &rvalue);
                    BinaryOperator::Addition
                },
                BinOp::Sub => {
                    // Add the overflow and underflow expression checks
                    wp = overflow::overflow_check(&wp, &var, binop, &lvalue, &rvalue);
                    BinaryOperator::Subtraction
                },
                BinOp::Mul => {
                    // Add the overflow and underflow expression checks
                    wp = overflow::overflow_check(&wp, &var, binop, &lvalue, &rvalue);
                    BinaryOperator::Multiplication
                },
                BinOp::Div => {
                    // Add the overflow and underflow expression checks, if operands are signed
                    if is_signed_type(determine_evaluation_type(&rvalue)) {
                        wp = overflow::overflow_check(&wp, &var, binop, &lvalue, &rvalue);
                    }
                    // Add the division by 0 expression check
                    wp = add_zero_check(&wp, &rvalue);
                    BinaryOperator::Division
                },
                BinOp::Rem => {
                    // Add the overflow and underflow expression checks, if operands are signed
                    if is_signed_type(determine_evaluation_type(&rvalue)) {
                        wp = overflow::overflow_check(&wp, &var, binop, &lvalue, &rvalue);
                    }
                    // Add the division by 0 expression check
                    wp = add_zero_check(&wp, &rvalue);
                    BinaryOperator::Modulo
                },
                BinOp::BitOr => BinaryOperator::BitwiseOr,
                BinOp::BitAnd => BinaryOperator::BitwiseAnd,
                BinOp::BitXor => BinaryOperator::BitwiseXor,
                BinOp::Shl => BinaryOperator::BitwiseLeftShift,
                BinOp::Shr => BinaryOperator::BitwiseRightShift,
                BinOp::Lt => BinaryOperator::LessThan,
                BinOp::Le => BinaryOperator::LessThanOrEqual,
                BinOp::Gt => BinaryOperator::GreaterThan,
                BinOp::Ge => BinaryOperator::GreaterThanOrEqual,
                BinOp::Eq => BinaryOperator::Equal,
                BinOp::Ne => BinaryOperator::NotEqual,
            };
            // Add the expression to the vector
            expression.push(Expression::BinaryExpression( BinaryExpressionData {
                op: op,
                left: Box::new(lvalue),
                right: Box::new(rvalue)
            } ));
        },
        // Generates Rvalue to a UnaryOp
        Rvalue::UnaryOp(ref unop, ref val) => {
            let exp: Expression = gen_expression(val, data);
            let op: UnaryOperator = match *unop {
                UnOp::Not => {
                    if determine_evaluation_type(&exp) == Types::Bool {
                        UnaryOperator::Not
                    } else {
                        UnaryOperator::BitwiseNot
                    }
                },
                UnOp::Neg => UnaryOperator::Negation,
            };
            // push the ne new exp onto the expression: Vec<>
            expression.push(Expression::UnaryExpression( UnaryExpressionData {
                op: op,
                e: Box::new(exp)
            } ));
        },
        //  FIXME: need def
        Rvalue::Use(ref operand) => {
            expression.push(gen_expression(operand, data));
        },
        //  FIXME: need def
        Rvalue::Aggregate(ref ag_kind, ref vec_operand) => {
            match *ag_kind {
                AggregateKind::Tuple => {
                    for operand in vec_operand.iter() {
                        let e = Expression::VariableMapping( VariableMappingData {
                            //name: var.name.as_str().to_string() + "." + i.to_string().as_str(),
                            name: format!("{:?}", operand),
                            var_type: gen_ty(operand, data)
                        } );
                        expression.push(e);
                    }
                },
                _ => rp_error!("Unsupported aggregate: only tuples are supported"),
            }
        },
        // FIXME: need def
        // Cast(ref cast_kind, ref cast_operand, ref cast_ty)
        Rvalue::Cast(..) => {
            expression.push(Expression::VariableMapping(var.clone()));
        },
        // FIXME: need def
        // Ref(ref ref_region, ref ref_borrow_kind, ref ref_lvalue) => {
        Rvalue::Ref(..) => {
            expression.push(Expression::VariableMapping(var.clone()));
        },
        // Unimplemented Rvalues
        Rvalue::Box(..) => unimplemented!(),
        Rvalue::Len(..) => unimplemented!(),
        _ => unimplemented!(),
    };

    // Replace any appearance of var in the weakest precondition with the expression
    for expr in &expression {
        substitute_variable_with_expression( &mut wp, &var, expr );
    }
    // Prints the new weakest precondition
    if debug {
        println!("new expression\t\t{:?}\n--------------------------------", wp.clone());
    }
    return Some(wp);
}

/// Returns the type of an operand as a `Types`
///
/// # Arguments:
/// * `operand` - The operand whose type is being returned.
/// * `data` - Contains the `BasicBlockData` and all argument, temp, and variable declarations from
///            the MIR pass.
///
/// # Remarks:
///
fn gen_ty(operand: &Operand, data: &mut MirData) -> Types {
    let type_string: String = match operand.clone() {
        Operand::Constant(ref constant) => constant.ty.to_string(),
        Operand::Consume(ref lvalue) => {
            match *lvalue {
                // Function argument
                Lvalue::Arg(ref arg) => data.arg_data[arg.index()].ty.to_string(),
                // Temporary variable
                Lvalue::Temp(ref temp) => data.temp_data[temp.index()].ty.to_string(),
                // Local variable
                Lvalue::Var(ref var) => data.var_data[var.index()].ty.to_string(),
                _ => unimplemented!(),
            }
        },
    };

    string_to_type(type_string)
}

/// Generates a version of wp "And"ed together with a conditional expression that mimics a check
/// to ensure division by 0 does not occur.
///
/// # Arguments:
/// * `wp` - The current weakest precondition that the "div by 0" is to be "And"ed to
/// * `exp` - The expression to check to make sure it is not divided by 0
///
/// # Return Value:
/// * Returns the modified weakest precondition with "div by 0" Expression "And"ed
///
/// # Remarks:
/// * Currently supported `ConstInt`: `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`
///
fn add_zero_check(wp: &Expression, exp: &Expression) -> Expression {
    let zero;
    if is_signed_type(determine_evaluation_type(exp)) {
        zero = Expression::SignedBitVector( SignedBitVectorData {
            // The bit-vector size of the given type
            size: match determine_evaluation_type(exp) {
                Types::I8 => 8,
                Types::I16 => 16,
                Types::I32 => 32,
                Types::I64 => 64,
                _ => rp_error!("Unimplemented checkeddAdd right-hand operand type"),
            },
            value: 0
        });
    } else {
        zero = Expression::UnsignedBitVector( UnsignedBitVectorData {
            // The bit-vector size of the given type
            size: match determine_evaluation_type(exp) {
                Types::U8 => 8,
                Types::U16 => 16,
                Types::U32 => 32,
                Types::U64 => 64,
                _ => rp_error!("Unimplemented checkeddAdd right-hand operand type"),
            },
            value: 0
        });
    }

    Expression::BinaryExpression( BinaryExpressionData{
        // And the weakest precondtion and the zero check
        op: BinaryOperator::And,
        left: Box::new(wp.clone()),
        right: Box::new(Expression::BinaryExpression( BinaryExpressionData{
            op: BinaryOperator::NotEqual,
            // The expression to be checked
            left: Box::new(exp.clone()),
            // Need to set appropriate type with value of 0
            right: Box::new(zero),
        }))
    })
}

/// Generates an appropriate variable mapping based on whatever variable, temp, or field is found
///
/// # Arguments:
/// * `lvalue` - The left value of an assignment to be generated into a `VariableMapping`
/// * `data` - Contains the `BasicBlockData` and all argument, temp, and variable declarations from
///            the MIR pass.
///
/// # Return Value:
/// * Returns a `VariableMappingData` that is built from `data` and `lvalue`
///
/// # Remarks:
///
fn gen_lvalue(lvalue: Lvalue, data: &mut MirData) -> VariableMappingData {
    match lvalue {
        // Function argument
        Lvalue::Arg(ref arg) => {
            // Find the name and type in the declaration
            VariableMappingData{
                name: data.arg_data[arg.index()].debug_name.as_str().to_string(),
                var_type: string_to_type(data.arg_data[arg.index()].ty.clone().to_string())
            }
        },
        // Temporary variable
        Lvalue::Temp(ref temp) => {
            // Find the index and type in the declaration
            let mut ty = data.temp_data[temp.index()].ty.clone().to_string();
            if let TypeVariants::TyTuple(t) = data.temp_data[temp.index()].ty.sty {
                if t.len() > 0 {
                    ty = t[0].to_string();
                }
            }
            VariableMappingData{
                name: "tmp".to_string() + temp.index().to_string().as_str(),
                var_type: string_to_type(ty)
            }
        },
        // Local variable
        Lvalue::Var(ref var) => {
            // Find the name and type in the declaration
            VariableMappingData{
                name: "var".to_string() + var.index().to_string().as_str(),
                var_type: string_to_type(data.var_data[var.index()].ty.clone().to_string())
            }
        },
        // The returned value
        Lvalue::ReturnPointer => {
            VariableMappingData{
                name: "return".to_string(),
                var_type: data.func_return_type.clone()
            }
        },
        // (Most likely) a field of a tuple from a checked operation
        Lvalue::Projection(pro) => {

            // Get the index
            let index: String = match pro.as_ref().elem.clone() {
                // Index(ref o)
                ProjectionElem::Index(_) => unimplemented!(),
                // Field(ref field, ref ty)
                ProjectionElem::Field(ref field, _) => (field.index() as i32).to_string(),
                _ => unimplemented!(),
            };

            // Get the name of the variable being projected
            let lvalue_name;
            let lvalue_type_string;

            match pro.as_ref().base {
                // Argument
                Lvalue::Arg(ref arg) => {
                    // Return the name of the argument
                    lvalue_name = data.arg_data[arg.index()].debug_name.as_str().to_string();
                    lvalue_type_string = data.arg_data[arg.index()].ty.clone().to_string();
                },
                // Temporary variable
                Lvalue::Temp(ref temp) => {
                    // Return "temp<index>"
                    lvalue_name = "tmp".to_string() + temp.index().to_string().as_str();

                    match data.temp_data[temp.index()].ty.sty {
                        TypeVariants::TyTuple(t) => lvalue_type_string = t[0].to_string(),
                        _ => unimplemented!(),
                    }
                },
                // Local variable
                Lvalue::Var(ref var) => {
                    // Return the name of the variable
                    let i = index.parse::<usize>().unwrap();
                    lvalue_name = "var".to_string() + var.index().to_string().as_str();

                    match data.var_data[var.index()].ty.sty {
                        TypeVariants::TyTuple(t) => lvalue_type_string = t[i].to_string(),
                        _ => unimplemented!(),
                    }
                },
                // Unimplemented Lvalue
                Lvalue::ReturnPointer => unimplemented!(),
                // Static(ref stat)
                Lvalue::Static(_) => unimplemented!(),
                // Multiply-nested projection
                Lvalue::Projection(_) => unimplemented!(),
            };

            // Get the index
            let index: String = match pro.as_ref().elem.clone() {

                // Field(ref field, ref ty)
                ProjectionElem::Field(ref field, _) => (field.index() as i32).to_string(),
                // Index(ref o)
                ProjectionElem::Index(_) => unimplemented!(),
                _ => unimplemented!(),
            };

            let lvalue_type: Types = string_to_type(lvalue_type_string);

            // Get the index int from index_operand, then stick it in the VariableMappingData
            VariableMappingData{ name: lvalue_name + "." + index.as_str(), var_type: lvalue_type }
        },
        _=> unimplemented!(),
    }
}

/// Generates an Expression based on some operand, either a literal or some kind of variable, temp,
/// or field
///
/// # Arguments:
/// * `operand` - The operand to generate a new expression from.
/// * `data` - Contains the `BasicBlockData` and all argument, temp, and variable declarations from
///            the MIR pass.
///
/// # Return Value:
/// * Returns a new expression generated from an operand
///
/// # Remarks:
/// * Current supported types: `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`, `bool`
///
fn gen_expression(operand: &Operand, data: &mut MirData) -> Expression {
    match *operand {
        // A variable/temp/field
        Operand::Consume (ref l) => {
            Expression::VariableMapping( gen_lvalue(l.clone(), data) )
        },
        // A literal value
        Operand::Constant (ref c) => {
            match c.literal {
                Literal::Value {ref value} => {
                    match *value {
                        ConstVal::Bool(ref const_bool) => {
                            Expression::BooleanLiteral(*const_bool)
                        }
                        ConstVal::Integral(ref const_int) => {
                            match *const_int {
                                ConstInt::I8(i) => {
                                    Expression::SignedBitVector( SignedBitVectorData {
                                        size: 8,
                                        value: i as i64
                                    } )
                                },
                                ConstInt::I16(i) => {
                                    Expression::SignedBitVector( SignedBitVectorData {
                                        size: 16,
                                        value: i as i64
                                    } )
                                },
                                ConstInt::I32(i) => {
                                    Expression::SignedBitVector( SignedBitVectorData {
                                        size: 32,
                                        value: i as i64
                                    } )
                                },
                                ConstInt::I64(i) => {
                                    Expression::SignedBitVector( SignedBitVectorData {
                                        size: 64,
                                        value: i as i64
                                    } )
                                },
                                ConstInt::U8(u) => {
                                    Expression::UnsignedBitVector( UnsignedBitVectorData {
                                        size: 8,
                                        value: u as u64
                                    } )
                                },
                                ConstInt::U16(u) => {
                                    Expression::UnsignedBitVector( UnsignedBitVectorData {
                                        size: 16,
                                        value: u as u64
                                    } )
                                },
                                ConstInt::U32(u) => {
                                    Expression::UnsignedBitVector( UnsignedBitVectorData {
                                        size: 32,
                                        value: u as u64
                                    } )
                                },
                                ConstInt::U64(u) => {
                                    Expression::UnsignedBitVector( UnsignedBitVectorData {
                                        size: 64,
                                        value: u as u64
                                    } )
                                },
                                _ => unimplemented!(),
                            }
                        },
                        _ => unimplemented!(),
                    }
                },
                // Item {ref def_id, ref substs}
                Literal::Item {..} => unimplemented!(),
                // Promoted {ref index}
                Literal::Promoted {..} => unimplemented!(),
            }
        },
    }
}
