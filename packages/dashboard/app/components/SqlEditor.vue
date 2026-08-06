<script setup lang="ts">
import { sql, MySQL } from '@codemirror/lang-sql'
import { EditorState } from '@codemirror/state'
import {
  EditorView,
  keymap,
  lineNumbers,
  highlightActiveLine,
  highlightActiveLineGutter,
} from '@codemirror/view'
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands'

const props = defineProps<{ modelValue: string }>()
const emit = defineEmits<{
  'update:modelValue': [value: string]
  run: []
}>()

const host = ref<HTMLElement>()
let view: EditorView | undefined

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
        sql({ dialect: MySQL }),
        keymap.of([
          {
            key: 'Mod-Enter',
            run: () => {
              emit('run')
              return true
            },
          },
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

onBeforeUnmount(() => view?.destroy())
</script>

<template>
  <div ref="host" class="min-h-68" />
</template>
