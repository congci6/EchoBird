import type { TKey } from '../../i18n';

/** The four client dialects plus "auto", shared by the picker and the probe
 *  report. Both read from this one list so a protocol can never be named two
 *  different ways in the same dialog. */
export const API_PROTOCOL_OPTIONS: { value: string; label: TKey; hint: TKey }[] = [
  { value: '', label: 'model.apiProtocolAuto', hint: 'model.apiProtocolAutoHint' },
  { value: 'openai-chat', label: 'model.apiProtocolChat', hint: 'model.apiProtocolChatHint' },
  {
    value: 'openai-responses',
    label: 'model.apiProtocolResponses',
    hint: 'model.apiProtocolResponsesHint',
  },
  {
    value: 'anthropic-messages',
    label: 'model.apiProtocolAnthropic',
    hint: 'model.apiProtocolAnthropicHint',
  },
  {
    value: 'gemini-generate-content',
    label: 'model.apiProtocolGemini',
    hint: 'model.apiProtocolGeminiHint',
  },
];
