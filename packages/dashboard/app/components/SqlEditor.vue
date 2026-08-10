<script setup lang="ts">
import { sql, MySQL } from '@codemirror/lang-sql'
import { autocompletion, completionKeymap } from '@codemirror/autocomplete'
import { EditorState, Compartment } from '@codemirror/state'
import {
  EditorView,
  keymap,
  lineNumbers,
  highlightActiveLine,
  highlightActiveLineGutter,
} from '@codemirror/view'
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands'

const props = defineProps<{ modelValue: string; schema?: Record<string, string[]> }>()
const emit = defineEmits<{
  'update:modelValue': [value: string]
  run: []
}>()

/// Reformats the buffer in place, preserving undo history.
///
/// sql-formatter is imported on demand rather than at module scope so it stays
/// out of the initial bundle, matching how the editor itself is lazy-loaded.
///
/// A parse failure leaves the text untouched. Formatting is a convenience, and
/// silently rewriting - or worse, emptying - a query someone is midway through
/// writing is a far more expensive failure than simply not reformatting it.
async function format() {
  if (!view) return
  const current = view.state.doc.toString()
  if (!current.trim()) return
  try {
    const { format: formatSql } = await import('sql-formatter')
    const formatted = formatSql(current, { language: 'mysql', keywordCase: 'upper' })
    if (formatted === current) return
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: formatted },
      // Park the cursor at the end rather than trying to map the old offset
      // through a reflow that moved every line.
      selection: { anchor: formatted.length },
    })
  } catch {
    // Unparseable SQL stays exactly as typed.
  }
}

defineExpose({ format })

const host = ref<HTMLElement>()
let view: EditorView | undefined

/// The SQL language extension is reconfigured rather than rebuilt when the
/// schema arrives. The metadata is fetched after mount, and recreating the
/// editor to apply it would discard whatever the user had already typed.
const language = new Compartment()

function languageWithSchema(schema?: Record<string, string[]>) {
  // lang-sql derives both table and column completion from this map, including
  // qualified `table.` lookups, so nothing here needs to parse SQL itself.
  return sql({ dialect: MySQL, schema: schema ?? {}, upperCaseKeywords: true })
}

onMounted(() => {
  if (!host.value) return
  view = new EditorView({
    parent: host.value,
    state: EditorState.create({
      doc: props.modelValue,
      extensions: [
        lineNumbers(),
        history(),
        highlightActiveLine(),
        highlightActiveLineGutter(),
        language.of(languageWithSchema(props.schema)),
        // activateOnTyping keeps the list out of the way until there is a
        // prefix to filter on: a popup on every keystroke in an empty editor
        // is noise, not help.
        autocompletion({ activateOnTyping: true, maxRenderedOptions: 20 }),
        keymap.of([
          {
            key: 'Mod-Enter',
            run: () => {
              emit('run')
              return true
            },
          },
          {
            // The convention every editor with a formatter uses.
            key: 'Shift-Alt-f',
            run: () => {
              void format()
              return true
            },
          },
          // Ahead of defaultKeymap so Escape and the arrows drive the
          // completion popup while it is open.
          ...completionKeymap,
          ...defaultKeymap,
          ...historyKeymap,
        ]),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) {
            emit('update:modelValue', update.state.doc.toString())
          }
        }),
        EditorView.theme({
          '&': {
            minHeight: '17rem',
            color: 'var(--text)',
            backgroundColor: 'transparent',
            fontSize: '0.86rem',
          },
          '.cm-content': {
            padding: '1rem 0',
            fontFamily: 'var(--mono)',
            caretColor: 'var(--signal)',
          },
          '.cm-gutters': {
            backgroundColor: 'transparent',
            color: 'var(--muted-text)',
            border: '0',
          },
          '.cm-activeLine, .cm-activeLineGutter': {
            backgroundColor: 'var(--panel-hover)',
          },
          '&.cm-focused': { outline: 'none' },
          '.cm-cursor': { borderLeftColor: 'var(--signal)' },
        }),
      ],
    }),
  })
})

watch(
  () => props.modelValue,
  (value) => {
    if (!view || value === view.state.doc.toString()) return
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: value },
    })
  },
)

watch(
  () => props.schema,
  (schema) => {
    view?.dispatch({ effects: language.reconfigure(languageWithSchema(schema)) })
  },
  { deep: true },
)

onBeforeUnmount(() => view?.destroy())
</script>

<template>
  <div ref="host" class="min-h-68" />
</template>
