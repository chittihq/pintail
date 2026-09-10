import { createApp, defineComponent, h, onMounted, onUnmounted } from 'vue'
import { componentToString } from '../app/components/ui/chart/utils'

// Run in a browser so detached Vue renders exercise real lifecycle cleanup.
export function checkTooltipLifecycle() {
  let mounted = 0
  let unmounted = 0
  let latest = ''
  let format: ReturnType<typeof componentToString>
  const tooltip = defineComponent({
    props: ['payload', 'x', 'config'],
    setup(props) {
      const listener = () => {}
      onMounted(() => {
        mounted += 1
        window.addEventListener('tooltip-test', listener)
      })
      onUnmounted(() => {
        unmounted += 1
        window.removeEventListener('tooltip-test', listener)
      })
      return () => h('span', `${props.payload.value}:${props.x}`)
    },
  })
  const app = createApp(defineComponent({
    setup() {
      format = componentToString({}, tooltip)
      return () => null
    },
  }))
  app.mount(document.createElement('div'))
  for (let value = 0; value < 200; value += 1) format!({ value }, value)
  latest = format!({ value: 199 }, 999)
  app.unmount()
  return { mounted, unmounted, latest }
}
