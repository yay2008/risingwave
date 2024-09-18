(self.webpackChunk_N_E=self.webpackChunk_N_E||[]).push([[27],{29882:function(e,n,t){(window.__NEXT_P=window.__NEXT_P||[]).push(["/subscriptions",function(){return t(87981)}])},3585:function(e,n,t){"use strict";t.d(n,{D:function(){return m},r:function(){return u}});var r=t(85893),i=t(79351),l=t(47741),a=t(41664),c=t.n(a),o=t(95100),s=t(33256),d=t(42215);function u(e){let[n,t]=(0,o.v1)("modalId",o.U);return[null==e?void 0:e.find(e=>e.id===n),t]}function m(e){let{modalData:n,onClose:t}=e;return(0,r.jsxs)(i.u_,{isOpen:void 0!==n,onClose:t,size:"3xl",children:[(0,r.jsx)(i.ZA,{}),(0,r.jsxs)(i.hz,{children:[(0,r.jsxs)(i.xB,{children:["Catalog of ",n&&(0,s.ks)(n)," ",null==n?void 0:n.id," - ",null==n?void 0:n.name]}),(0,r.jsx)(i.ol,{}),(0,r.jsx)(i.fe,{children:n&&(0,r.jsx)(d.Rm,{src:n,collapsed:1,name:null,displayDataTypes:!1})}),(0,r.jsxs)(i.mz,{children:[n&&(0,s.vx)(n)&&(0,r.jsx)(l.zx,{colorScheme:"blue",mr:3,children:(0,r.jsx)(c(),{href:"/fragment_graph/?id=".concat(n.id),children:"View Fragments"})}),(0,r.jsx)(l.zx,{mr:3,onClick:t,children:"Close"})]})]})]})}},42215:function(e,n,t){"use strict";t.d(n,{Rm:function(){return z},KB:function(){return W},Kf:function(){return I},gU:function(){return $},vk:function(){return D},vP:function(){return G},sW:function(){return P}});var r=t(85893),i=t(47741),l=t(40639),a=t(67294),c=t(32067),o=t(54520),s=t(28387),d=(...e)=>e.filter(Boolean).join(" "),[u,m]=(0,s.k)({name:"TableStylesContext",errorMessage:"useTableStyles returned is 'undefined'. Seems you forgot to wrap the components in \"<Table />\" "}),h=(0,c.Gp)((e,n)=>{let t=(0,c.jC)("Table",e),{className:r,...i}=(0,o.Lr)(e);return a.createElement(u,{value:t},a.createElement(c.m$.table,{role:"table",ref:n,__css:t.table,className:d("chakra-table",r),...i}))});h.displayName="Table";var p=(0,c.Gp)((e,n)=>{let{overflow:t,overflowX:r,className:i,...l}=e;return a.createElement(c.m$.div,{ref:n,className:d("chakra-table__container",i),...l,__css:{display:"block",whiteSpace:"nowrap",WebkitOverflowScrolling:"touch",overflowX:t??r??"auto",overflowY:"hidden",maxWidth:"100%"}})});(0,c.Gp)((e,n)=>{let{placement:t="bottom",...r}=e,i=m();return a.createElement(c.m$.caption,{...r,ref:n,__css:{...i.caption,captionSide:t}})}).displayName="TableCaption";var x=(0,c.Gp)((e,n)=>{let t=m();return a.createElement(c.m$.thead,{...e,ref:n,__css:t.thead})}),f=(0,c.Gp)((e,n)=>{let t=m();return a.createElement(c.m$.tbody,{...e,ref:n,__css:t.tbody})});(0,c.Gp)((e,n)=>{let t=m();return a.createElement(c.m$.tfoot,{...e,ref:n,__css:t.tfoot})});var v=(0,c.Gp)(({isNumeric:e,...n},t)=>{let r=m();return a.createElement(c.m$.th,{...n,ref:t,__css:r.th,"data-is-numeric":e})}),j=(0,c.Gp)((e,n)=>{let t=m();return a.createElement(c.m$.tr,{role:"row",...e,ref:n,__css:t.tr})}),w=(0,c.Gp)(({isNumeric:e,...n},t)=>{let r=m();return a.createElement(c.m$.td,{role:"gridcell",...n,ref:t,__css:r.td,"data-is-numeric":e})}),_=t(63679),b=t(9008),k=t.n(b),y=t(41664),g=t.n(y),C=t(64030),E=t(44599),N=t(33256);function S(e){var n,t,r,i;return"columnDesc"in e?"".concat(null===(n=e.columnDesc)||void 0===n?void 0:n.name," (").concat(null===(r=e.columnDesc)||void 0===r?void 0:null===(t=r.columnType)||void 0===t?void 0:t.typeName,")"):"".concat(e.name," (").concat(null===(i=e.dataType)||void 0===i?void 0:i.typeName,")")}var T=t(3585);let z=(0,_.ZP)(()=>t.e(171).then(t.t.bind(t,55171,23))),D={name:"Depends",width:1,content:e=>(0,r.jsx)(g(),{href:"/dependency_graph/?id=".concat(e.id),children:(0,r.jsx)(i.zx,{size:"sm","aria-label":"view dependents",colorScheme:"blue",variant:"link",children:"D"})})},G={name:"Primary Key",width:1,content:e=>e.pk.map(e=>e.columnIndex).map(n=>e.columns[n]).map(e=>S(e)).join(", ")},$={name:"Connector",width:3,content:e=>{var n;return null!==(n=e.withProperties.connector)&&void 0!==n?n:"unknown"}},I={name:"Connector",width:3,content:e=>{var n;return null!==(n=e.properties.connector)&&void 0!==n?n:"unknown"}},P=[D,{name:"Fragments",width:1,content:e=>(0,r.jsx)(g(),{href:"/fragment_graph/?id=".concat(e.id),children:(0,r.jsx)(i.zx,{size:"sm","aria-label":"view fragments",colorScheme:"blue",variant:"link",children:"F"})})}];function W(e,n,t){let{response:c}=(0,E.Z)(async()=>{let e=await n(),t=await (0,N.Rf)(),r=await (0,N.Cp)(),i=await (0,N.jW)();return e.map(e=>{let n=t.find(n=>n.id===e.owner),l=null==n?void 0:n.name,a=i.find(n=>n.id===e.schemaId),c=null==a?void 0:a.name,o=r.find(n=>n.id===e.databaseId),s=null==o?void 0:o.name;return{...e,ownerName:l,schemaName:c,databaseName:s}})}),[o,s]=(0,T.r)(c),d=(0,r.jsx)(T.D,{modalData:o,onClose:()=>s(null)}),u=(0,r.jsxs)(l.xu,{p:3,children:[(0,r.jsx)(C.Z,{children:e}),(0,r.jsx)(p,{children:(0,r.jsxs)(h,{variant:"simple",size:"sm",maxWidth:"full",children:[(0,r.jsx)(x,{children:(0,r.jsxs)(j,{children:[(0,r.jsx)(v,{width:3,children:"Id"}),(0,r.jsx)(v,{width:5,children:"Database"}),(0,r.jsx)(v,{width:5,children:"Schema"}),(0,r.jsx)(v,{width:5,children:"Name"}),(0,r.jsx)(v,{width:3,children:"Owner"}),t.map(e=>(0,r.jsx)(v,{width:e.width,children:e.name},e.name)),(0,r.jsx)(v,{children:"Visible Columns"})]})}),(0,r.jsx)(f,{children:null==c?void 0:c.map(e=>(0,r.jsxs)(j,{children:[(0,r.jsx)(w,{children:(0,r.jsx)(i.zx,{size:"sm","aria-label":"view catalog",colorScheme:"blue",variant:"link",onClick:()=>s(e.id),children:e.id})}),(0,r.jsx)(w,{children:e.databaseName}),(0,r.jsx)(w,{children:e.schemaName}),(0,r.jsx)(w,{children:e.name}),(0,r.jsx)(w,{children:e.ownerName}),t.map(n=>(0,r.jsx)(w,{children:n.content(e)},n.name)),e.columns&&e.columns.length>0&&(0,r.jsx)(w,{overflowWrap:"normal",children:e.columns.filter(e=>!("isHidden"in e)||!e.isHidden).map(e=>S(e)).join(", ")})]},e.id))})]})})]});return(0,r.jsxs)(a.Fragment,{children:[(0,r.jsx)(k(),{children:(0,r.jsx)("title",{children:e})}),d,u]})}},87981:function(e,n,t){"use strict";t.r(n),t.d(n,{default:function(){return l}});var r=t(42215),i=t(33256);function l(){return(0,r.KB)("Subscriptions",i.tJ,[{name:"Retention Seconds",width:3,content:e=>{var n;return null!==(n=e.retentionSeconds)&&void 0!==n?n:"unknown"}},{name:"Dependent Table Id",width:3,content:e=>{var n;return null!==(n=e.dependentTableId)&&void 0!==n?n:"unknown"}}])}}},function(e){e.O(0,[662,679,184,667,721,371,888,774,179],function(){return e(e.s=29882)}),_N_E=e.O()}]);