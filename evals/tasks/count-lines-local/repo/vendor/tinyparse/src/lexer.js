// tinyparse: lexer.js
export function lexerStep1(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep2(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep3(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep4(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep5(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep6(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep7(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep8(input, pos) {
  const ch = input[pos + 4];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep9(input, pos) {
  const ch = input[pos + 9];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep10(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep11(input, pos) {
  const ch = input[pos + 9];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep12(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep13(input, pos) {
  const ch = input[pos + 6];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep14(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep15(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep16(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep17(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep18(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep19(input, pos) {
  const ch = input[pos + 9];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep20(input, pos) {
  const ch = input[pos + 1];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep21(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep22(input, pos) {
  const ch = input[pos + 4];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep23(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep24(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep25(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep26(input, pos) {
  const ch = input[pos + 1];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep27(input, pos) {
  const ch = input[pos + 9];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep28(input, pos) {
  const ch = input[pos + 6];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep29(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep30(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep31(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep32(input, pos) {
  const ch = input[pos + 6];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep33(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep34(input, pos) {
  const ch = input[pos + 9];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep35(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep36(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep37(input, pos) {
  const ch = input[pos + 6];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep38(input, pos) {
  const ch = input[pos + 1];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep39(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep40(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep41(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep42(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep43(input, pos) {
  const ch = input[pos + 4];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep44(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep45(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep46(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep47(input, pos) {
  const ch = input[pos + 6];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep48(input, pos) {
  const ch = input[pos + 6];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep49(input, pos) {
  const ch = input[pos + 9];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep50(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep51(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep52(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep53(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep54(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep55(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep56(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep57(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep58(input, pos) {
  const ch = input[pos + 4];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep59(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 2 };
}

export function lexerStep60(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep61(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep62(input, pos) {
  const ch = input[pos + 4];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep63(input, pos) {
  const ch = input[pos + 4];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep64(input, pos) {
  const ch = input[pos + 1];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep65(input, pos) {
  const ch = input[pos + 1];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 1 };
}

export function lexerStep66(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep67(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 3 };
}

export function lexerStep68(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'lexer', value: ch, end: pos + 4 };
}

export function lexerStep69(input, pos) {
  const ch = input[pos + 2];
// end
