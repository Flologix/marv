// tinyparse: parser.js
export function parserStep1(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep2(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep3(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep4(input, pos) {
  const ch = input[pos + 4];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep5(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep6(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep7(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep8(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep9(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep10(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep11(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep12(input, pos) {
  const ch = input[pos + 4];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep13(input, pos) {
  const ch = input[pos + 4];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep14(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep15(input, pos) {
  const ch = input[pos + 4];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep16(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep17(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep18(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep19(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep20(input, pos) {
  const ch = input[pos + 9];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep21(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep22(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep23(input, pos) {
  const ch = input[pos + 4];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep24(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep25(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep26(input, pos) {
  const ch = input[pos + 6];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep27(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep28(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep29(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep30(input, pos) {
  const ch = input[pos + 6];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep31(input, pos) {
  const ch = input[pos + 1];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep32(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep33(input, pos) {
  const ch = input[pos + 1];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep34(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep35(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep36(input, pos) {
  const ch = input[pos + 1];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep37(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep38(input, pos) {
  const ch = input[pos + 4];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep39(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep40(input, pos) {
  const ch = input[pos + 6];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep41(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep42(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep43(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep44(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep45(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep46(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep47(input, pos) {
  const ch = input[pos + 9];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep48(input, pos) {
  const ch = input[pos + 6];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep49(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep50(input, pos) {
  const ch = input[pos + 6];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep51(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep52(input, pos) {
  const ch = input[pos + 1];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep53(input, pos) {
  const ch = input[pos + 1];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep54(input, pos) {
  const ch = input[pos + 9];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep55(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep56(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep57(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep58(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep59(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep60(input, pos) {
  const ch = input[pos + 6];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep61(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep62(input, pos) {
  const ch = input[pos + 4];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep63(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep64(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep65(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep66(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep67(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep68(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep69(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep70(input, pos) {
  const ch = input[pos + 1];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep71(input, pos) {
  const ch = input[pos + 6];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep72(input, pos) {
  const ch = input[pos + 1];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep73(input, pos) {
  const ch = input[pos + 8];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep74(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep75(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep76(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep77(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep78(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep79(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep80(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep81(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep82(input, pos) {
  const ch = input[pos + 7];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep83(input, pos) {
  const ch = input[pos + 2];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep84(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep85(input, pos) {
  const ch = input[pos + 1];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep86(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 1 };
}

export function parserStep87(input, pos) {
  const ch = input[pos + 5];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 4 };
}

export function parserStep88(input, pos) {
  const ch = input[pos + 3];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 3 };
}

export function parserStep89(input, pos) {
  const ch = input[pos + 0];
  if (ch === undefined) return null;
  return { kind: 'parser', value: ch, end: pos + 2 };
}

export function parserStep90(input, pos) {
  const ch = input[pos + 9];
// end
